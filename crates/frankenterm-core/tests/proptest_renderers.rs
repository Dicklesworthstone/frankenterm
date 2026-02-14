//! Property-based tests for the output renderers module.
//!
//! Tests serde roundtrip invariants of AnalyticsSummaryData, RuleDetail,
//! RuleListItem, RuleTestMatch, and HealthDiagnosticStatus equality.

use frankenterm_core::output::{
    AnalyticsSummaryData, HealthDiagnosticStatus, RuleDetail, RuleListItem, RuleTestMatch,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_analytics_summary() -> impl Strategy<Value = AnalyticsSummaryData> {
    (
        "[a-zA-Z 0-9]{1,30}",   // period_label
        any::<i64>(),            // total_tokens
        (0.0f64..100_000.0),     // total_cost
        any::<i64>(),            // rate_limit_hits
        any::<i64>(),            // workflow_runs
    )
        .prop_map(|(period_label, total_tokens, total_cost, rate_limit_hits, workflow_runs)| {
            AnalyticsSummaryData {
                period_label,
                total_tokens,
                total_cost,
                rate_limit_hits,
                workflow_runs,
            }
        })
}

fn arb_rule_list_item() -> impl Strategy<Value = RuleListItem> {
    (
        "[a-z._]{1,30}",           // id
        "[a-z_]{1,15}",            // agent_type
        "[a-z._]{1,20}",          // event_type
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("critical".to_string()),
        ],
        "[a-zA-Z ]{1,50}",         // description
        prop::option::of("[a-z_]{1,20}"), // workflow
        0usize..10,                // anchor_count
        any::<bool>(),             // has_regex
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
        "[a-z._]{1,30}",           // rule_id
        "[a-z_]{1,15}",            // agent_type
        "[a-z._]{1,20}",          // event_type
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("critical".to_string()),
        ],
        (0.0f64..1.0),             // confidence
        "[a-zA-Z0-9 ]{1,50}",     // matched_text
    )
        .prop_map(
            |(rule_id, agent_type, event_type, severity, confidence, matched_text)| {
                RuleTestMatch {
                    rule_id,
                    agent_type,
                    event_type,
                    severity,
                    confidence,
                    matched_text,
                    extracted: None,
                }
            },
        )
}

fn arb_rule_detail() -> impl Strategy<Value = RuleDetail> {
    (
        "[a-z._]{1,30}",           // id
        "[a-z_]{1,15}",            // agent_type
        "[a-z._]{1,20}",          // event_type
        prop_oneof![
            Just("info".to_string()),
            Just("warning".to_string()),
            Just("critical".to_string()),
        ],
        "[a-zA-Z ]{1,50}",         // description
        proptest::collection::vec("[a-z ]{1,20}", 0..5), // anchors
        prop::option::of("[a-z.*]+"),                    // regex
        prop::option::of("[a-z_]{1,20}"),                // workflow
        prop::option::of("[a-zA-Z .]{1,30}"),            // remediation
        prop::option::of("[a-zA-Z .]{1,30}"),            // manual_fix
        prop::option::of("https://[a-z.]+/[a-z/]+"),     // learn_more_url
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

fn arb_health_status() -> impl Strategy<Value = HealthDiagnosticStatus> {
    prop_oneof![
        Just(HealthDiagnosticStatus::Ok),
        Just(HealthDiagnosticStatus::Info),
        Just(HealthDiagnosticStatus::Warning),
        Just(HealthDiagnosticStatus::Error),
    ]
}

// ── Helpers ─────────────────────────────────────────────────────────────────

fn f64_approx_eq(a: f64, b: f64) -> bool {
    if a == b {
        return true;
    }
    let diff = (a - b).abs();
    let max_val = a.abs().max(b.abs());
    if max_val == 0.0 {
        diff < 1e-15
    } else {
        diff / max_val < 1e-12
    }
}

// ── AnalyticsSummaryData: serde ─────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all fields (f64 with tolerance).
    #[test]
    fn analytics_summary_serde_roundtrip(data in arb_analytics_summary()) {
        let json = serde_json::to_string(&data).unwrap();
        let parsed: AnalyticsSummaryData = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.period_label.as_str(), data.period_label.as_str());
        prop_assert_eq!(parsed.total_tokens, data.total_tokens);
        prop_assert!(f64_approx_eq(parsed.total_cost, data.total_cost),
            "cost: {} vs {}", parsed.total_cost, data.total_cost);
        prop_assert_eq!(parsed.rate_limit_hits, data.rate_limit_hits);
        prop_assert_eq!(parsed.workflow_runs, data.workflow_runs);
    }

    /// Serialized analytics is valid JSON object.
    #[test]
    fn analytics_summary_valid_json(data in arb_analytics_summary()) {
        let json = serde_json::to_string(&data).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Required fields present in JSON.
    #[test]
    fn analytics_summary_has_required_fields(data in arb_analytics_summary()) {
        let json = serde_json::to_string(&data).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("period_label").is_some());
        prop_assert!(value.get("total_tokens").is_some());
        prop_assert!(value.get("total_cost").is_some());
        prop_assert!(value.get("rate_limit_hits").is_some());
        prop_assert!(value.get("workflow_runs").is_some());
    }

    /// Clone produces equivalent analytics.
    #[test]
    fn analytics_summary_clone(data in arb_analytics_summary()) {
        let cloned = data.clone();
        prop_assert_eq!(cloned.period_label.as_str(), data.period_label.as_str());
        prop_assert_eq!(cloned.total_tokens, data.total_tokens);
        prop_assert_eq!(cloned.workflow_runs, data.workflow_runs);
    }
}

// ── RuleListItem: serde ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all fields.
    #[test]
    fn rule_list_item_serde_roundtrip(item in arb_rule_list_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let parsed: RuleListItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.id.as_str(), item.id.as_str());
        prop_assert_eq!(parsed.agent_type.as_str(), item.agent_type.as_str());
        prop_assert_eq!(parsed.event_type.as_str(), item.event_type.as_str());
        prop_assert_eq!(parsed.severity.as_str(), item.severity.as_str());
        prop_assert_eq!(parsed.description.as_str(), item.description.as_str());
        prop_assert_eq!(parsed.workflow, item.workflow);
        prop_assert_eq!(parsed.anchor_count, item.anchor_count);
        prop_assert_eq!(parsed.has_regex, item.has_regex);
    }

    /// Serialized item is valid JSON object.
    #[test]
    fn rule_list_item_valid_json(item in arb_rule_list_item()) {
        let json = serde_json::to_string(&item).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Pretty-printed JSON also roundtrips.
    #[test]
    fn rule_list_item_pretty_roundtrip(item in arb_rule_list_item()) {
        let json = serde_json::to_string_pretty(&item).unwrap();
        let parsed: RuleListItem = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.id.as_str(), item.id.as_str());
        prop_assert_eq!(parsed.anchor_count, item.anchor_count);
        prop_assert_eq!(parsed.has_regex, item.has_regex);
    }
}

// ── RuleTestMatch: serde ────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all fields (f64 with tolerance).
    #[test]
    fn rule_test_match_serde_roundtrip(m in arb_rule_test_match()) {
        let json = serde_json::to_string(&m).unwrap();
        let parsed: RuleTestMatch = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.rule_id.as_str(), m.rule_id.as_str());
        prop_assert_eq!(parsed.agent_type.as_str(), m.agent_type.as_str());
        prop_assert_eq!(parsed.event_type.as_str(), m.event_type.as_str());
        prop_assert_eq!(parsed.severity.as_str(), m.severity.as_str());
        prop_assert!(f64_approx_eq(parsed.confidence, m.confidence),
            "confidence: {} vs {}", parsed.confidence, m.confidence);
        prop_assert_eq!(parsed.matched_text.as_str(), m.matched_text.as_str());
        prop_assert_eq!(parsed.extracted, m.extracted);
    }

    /// Serialized match is valid JSON object.
    #[test]
    fn rule_test_match_valid_json(m in arb_rule_test_match()) {
        let json = serde_json::to_string(&m).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Confidence field is preserved in JSON as a number.
    #[test]
    fn rule_test_match_confidence_is_number(m in arb_rule_test_match()) {
        let json = serde_json::to_string(&m).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.get("confidence").unwrap().is_f64() ||
                     value.get("confidence").unwrap().is_i64(),
            "confidence should be a number in JSON");
    }
}

// ── RuleDetail: serde ───────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// JSON serde roundtrip preserves all fields.
    #[test]
    fn rule_detail_serde_roundtrip(detail in arb_rule_detail()) {
        let json = serde_json::to_string(&detail).unwrap();
        let parsed: RuleDetail = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(parsed.id.as_str(), detail.id.as_str());
        prop_assert_eq!(parsed.agent_type.as_str(), detail.agent_type.as_str());
        prop_assert_eq!(parsed.event_type.as_str(), detail.event_type.as_str());
        prop_assert_eq!(parsed.severity.as_str(), detail.severity.as_str());
        prop_assert_eq!(parsed.description.as_str(), detail.description.as_str());
        prop_assert_eq!(parsed.anchors, detail.anchors);
        prop_assert_eq!(parsed.regex, detail.regex);
        prop_assert_eq!(parsed.workflow, detail.workflow);
        prop_assert_eq!(parsed.remediation, detail.remediation);
        prop_assert_eq!(parsed.manual_fix, detail.manual_fix);
        prop_assert_eq!(parsed.learn_more_url, detail.learn_more_url);
    }

    /// Serialized detail is valid JSON object.
    #[test]
    fn rule_detail_valid_json(detail in arb_rule_detail()) {
        let json = serde_json::to_string(&detail).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        prop_assert!(value.is_object());
    }

    /// Anchors field serializes as a JSON array.
    #[test]
    fn rule_detail_anchors_is_array(detail in arb_rule_detail()) {
        let json = serde_json::to_string(&detail).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let anchors = value.get("anchors").unwrap();
        prop_assert!(anchors.is_array(),
            "anchors should be an array, got: {}", anchors);
        let arr = anchors.as_array().unwrap();
        prop_assert_eq!(arr.len(), detail.anchors.len(),
            "anchor count mismatch");
    }

    /// Optional fields (regex, workflow, etc.) are present/absent correctly.
    #[test]
    fn rule_detail_optional_fields(detail in arb_rule_detail()) {
        let json = serde_json::to_string(&detail).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();

        // regex: always present in JSON (None serializes as null)
        let regex_val = value.get("regex");
        prop_assert!(regex_val.is_some(), "regex field should be present");
        if detail.regex.is_none() {
            prop_assert!(regex_val.unwrap().is_null(),
                "regex should be null when None");
        } else {
            prop_assert!(regex_val.unwrap().is_string(),
                "regex should be string when Some");
        }
    }

    /// Clone produces equivalent detail.
    #[test]
    fn rule_detail_clone(detail in arb_rule_detail()) {
        let cloned = detail.clone();
        prop_assert_eq!(cloned.id.as_str(), detail.id.as_str());
        prop_assert_eq!(cloned.anchors, detail.anchors);
        prop_assert_eq!(cloned.regex, detail.regex);
        prop_assert_eq!(cloned.learn_more_url, detail.learn_more_url);
    }
}

// ── HealthDiagnosticStatus: equality ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Reflexivity: every status equals itself.
    #[test]
    fn health_status_reflexive(s in arb_health_status()) {
        prop_assert_eq!(s, s);
    }

    /// Symmetry: if a == b then b == a.
    #[test]
    fn health_status_symmetric(a in arb_health_status(), b in arb_health_status()) {
        if a == b {
            prop_assert_eq!(b, a);
        }
    }

    /// Copy semantics work.
    #[test]
    fn health_status_copy(s in arb_health_status()) {
        let copied = s;
        prop_assert_eq!(s, copied);
    }

    /// Debug format is non-empty.
    #[test]
    fn health_status_debug(s in arb_health_status()) {
        let debug = format!("{:?}", s);
        prop_assert!(!debug.is_empty());
    }

    /// Four distinct variants exist.
    #[test]
    fn health_status_distinct_variants(_i in 0..1u8) {
        let ok = HealthDiagnosticStatus::Ok;
        let info = HealthDiagnosticStatus::Info;
        let warn = HealthDiagnosticStatus::Warning;
        let error = HealthDiagnosticStatus::Error;
        prop_assert_ne!(ok, info);
        prop_assert_ne!(ok, warn);
        prop_assert_ne!(ok, error);
        prop_assert_ne!(info, warn);
        prop_assert_ne!(info, error);
        prop_assert_ne!(warn, error);
    }
}
