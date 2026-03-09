//! Property-based tests for the workflows/engine module.
//!
//! Covers WorkflowStepPolicyDecision, WorkflowStepPolicySummary, and
//! policy_summary_decision_is_allow: serde roundtrip, parse consistency,
//! is_allowed invariant, and redact_text_for_log length guarantees.
//!
//! Complements proptest_workflows.rs (StepResult, WaitCondition, locks) and
//! proptest_workflows_expanded.rs (DescriptorStep, ExecutionStatus, UnstickReport).

use frankenterm_core::policy::ActionKind;
use frankenterm_core::workflows::{
    WorkflowStepPolicyDecision, WorkflowStepPolicySummary, policy_summary_decision_is_allow,
    redact_text_for_log,
};
use proptest::prelude::*;

// ── Strategies ──────────────────────────────────────────────────────────────

fn arb_policy_decision() -> impl Strategy<Value = WorkflowStepPolicyDecision> {
    prop_oneof![
        Just(WorkflowStepPolicyDecision::Allow),
        Just(WorkflowStepPolicyDecision::Deny),
        Just(WorkflowStepPolicyDecision::RequireApproval),
        Just(WorkflowStepPolicyDecision::Error),
    ]
}

fn arb_action_kind() -> impl Strategy<Value = ActionKind> {
    prop_oneof![
        Just(ActionKind::SendText),
        Just(ActionKind::SendCtrlC),
        Just(ActionKind::SendCtrlD),
        Just(ActionKind::SendCtrlZ),
        Just(ActionKind::SendControl),
        Just(ActionKind::Spawn),
        Just(ActionKind::Split),
        Just(ActionKind::Activate),
        Just(ActionKind::Close),
        Just(ActionKind::BrowserAuth),
        Just(ActionKind::WorkflowRun),
        Just(ActionKind::ReservePane),
        Just(ActionKind::ReleasePane),
        Just(ActionKind::ReadOutput),
        Just(ActionKind::SearchOutput),
        Just(ActionKind::WriteFile),
        Just(ActionKind::DeleteFile),
        Just(ActionKind::ExecCommand),
        Just(ActionKind::ConnectorNotify),
        Just(ActionKind::ConnectorTicket),
        Just(ActionKind::ConnectorTriggerWorkflow),
        Just(ActionKind::ConnectorAuditLog),
        Just(ActionKind::ConnectorInvoke),
        Just(ActionKind::ConnectorCredentialAction),
    ]
}

fn arb_policy_summary() -> impl Strategy<Value = WorkflowStepPolicySummary> {
    (
        arb_policy_decision(),
        proptest::option::of(arb_action_kind()),
        proptest::option::of("[a-z._]{1,32}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,64}"),
    )
        .prop_map(|(decision, action, rule_id, reason, summary, error)| {
            WorkflowStepPolicySummary {
                decision,
                action,
                rule_id,
                reason,
                summary,
                error,
                decision_context: None,
            }
        })
}

// ── WorkflowStepPolicyDecision ──────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 1: All WorkflowStepPolicyDecision variants survive serde roundtrip.
    #[test]
    fn policy_decision_serde_roundtrip(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        let parsed: WorkflowStepPolicyDecision = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(decision, parsed);
    }

    // Property 2: is_allowed is true IFF decision is Allow.
    #[test]
    fn policy_decision_is_allowed_iff_allow(decision in arb_policy_decision()) {
        let expected = matches!(decision, WorkflowStepPolicyDecision::Allow);
        prop_assert_eq!(decision.is_allowed(), expected);
    }

    // Property 3: Decision JSON is always a quoted snake_case string.
    #[test]
    fn policy_decision_json_is_quoted_string(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        prop_assert!(json.starts_with('"'));
        prop_assert!(json.ends_with('"'));
        let inner = &json[1..json.len()-1];
        let valid = ["allow", "deny", "require_approval", "error"];
        let check = valid.contains(&inner);
        prop_assert!(check, "unexpected decision JSON: {}", json);
    }
}

// ── WorkflowStepPolicySummary ───────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 4: WorkflowStepPolicySummary survives serde roundtrip.
    #[test]
    fn policy_summary_serde_roundtrip(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: WorkflowStepPolicySummary = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(summary.decision, parsed.decision);
        prop_assert_eq!(summary.action, parsed.action);
        prop_assert_eq!(summary.rule_id, parsed.rule_id);
        prop_assert_eq!(summary.reason, parsed.reason);
        prop_assert_eq!(summary.summary, parsed.summary);
        prop_assert_eq!(summary.error, parsed.error);
    }

    // Property 5: WorkflowStepPolicySummary::parse inverts to_string.
    #[test]
    fn policy_summary_parse_inverts_serialize(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json);
        prop_assert!(parsed.is_some(), "parse failed on valid JSON: {}", json);
        let parsed = parsed.unwrap();
        prop_assert_eq!(summary.decision, parsed.decision);
        prop_assert_eq!(summary.action, parsed.action);
        prop_assert_eq!(summary.rule_id, parsed.rule_id);
    }

    // Property 6: is_allowed on summary matches decision.is_allowed.
    #[test]
    fn policy_summary_is_allowed_delegates_to_decision(summary in arb_policy_summary()) {
        prop_assert_eq!(summary.is_allowed(), summary.decision.is_allowed());
    }

    // Property 7: policy_summary_decision_is_allow agrees with typed parse.
    #[test]
    fn policy_summary_decision_fn_agrees_with_parse(summary in arb_policy_summary()) {
        let json = serde_json::to_string(&summary).unwrap();
        let typed_result = summary.decision.is_allowed();
        let fn_result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(fn_result, Some(typed_result));
    }

    // Property 8: Optional fields serialize to absent keys (skip_serializing_if).
    #[test]
    fn policy_summary_none_fields_omitted(decision in arb_policy_decision()) {
        let summary = WorkflowStepPolicySummary {
            decision,
            action: None,
            rule_id: None,
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = parsed.as_object().unwrap();
        // Only the "decision" key should be present; all optional fields omitted.
        prop_assert!(!obj.contains_key("action"), "action should be omitted: {}", json);
        prop_assert!(!obj.contains_key("rule_id"), "rule_id should be omitted: {}", json);
        prop_assert!(!obj.contains_key("reason"), "reason should be omitted: {}", json);
        prop_assert!(!obj.contains_key("summary"), "summary should be omitted: {}", json);
        prop_assert!(!obj.contains_key("error"), "error should be omitted: {}", json);
        prop_assert!(!obj.contains_key("decision_context"), "decision_context should be omitted: {}", json);
        prop_assert_eq!(obj.len(), 1, "only 'decision' key expected: {}", json);
    }
}

// ── policy_summary_decision_is_allow ────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 9: Legacy untyped JSON with "decision":"allow" returns Some(true).
    #[test]
    fn legacy_json_allow_returns_true(
        extra_key in "[a-z]{1,8}",
        extra_val in "[a-z]{1,8}",
    ) {
        let json = format!(r#"{{"decision":"allow","{}":"{}"}}"#, extra_key, extra_val);
        let result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(result, Some(true));
    }

    // Property 10: Legacy untyped JSON with "decision":"deny" returns Some(false).
    #[test]
    fn legacy_json_deny_returns_false(
        extra_key in "[a-z]{1,8}",
        extra_val in "[a-z]{1,8}",
    ) {
        let json = format!(r#"{{"decision":"deny","{}":"{}"}}"#, extra_key, extra_val);
        let result = policy_summary_decision_is_allow(&json);
        prop_assert_eq!(result, Some(false));
    }

    // Property 11: Non-JSON input returns None.
    #[test]
    fn non_json_returns_none(input in "[^{\"]*") {
        // Filter out accidental valid JSON
        if serde_json::from_str::<serde_json::Value>(&input).is_err() {
            let result = policy_summary_decision_is_allow(&input);
            prop_assert_eq!(result, None);
        }
    }
}

// ── redact_text_for_log ─────────────────────────────────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(500))]

    // Property 12: Output char count never exceeds max_len + 3 (for "..." suffix).
    #[test]
    fn redact_output_length_bounded(
        text in ".{0,256}",
        max_len in 1usize..=256,
    ) {
        let result = redact_text_for_log(&text, max_len);
        let char_count = result.chars().count();
        // If truncated, output is max_len chars + "..."
        prop_assert!(
            char_count <= max_len + 3,
            "output too long: {} chars for max_len={}, output={:?}",
            char_count, max_len, result
        );
    }

    // Property 13: Short text passes through unmodified (after redaction).
    #[test]
    fn redact_short_text_no_truncation(
        text in "[a-zA-Z0-9 ]{0,20}",
        max_len in 20usize..=100,
    ) {
        let result = redact_text_for_log(&text, max_len);
        // The redactor won't find secrets in alphanumeric text,
        // so the output should match the input exactly.
        prop_assert_eq!(result, text);
    }

    // Property 14: Truncated output always ends with "...".
    #[test]
    fn redact_truncated_has_ellipsis(
        text in ".{10,256}",
        max_len in 1usize..=5,
    ) {
        let result = redact_text_for_log(&text, max_len);
        // If the redacted text was longer than max_len, it gets truncated
        if result.len() > max_len {
            prop_assert!(
                result.ends_with("..."),
                "truncated output should end with '...': {:?}", result
            );
        }
    }

    // Property 15: Empty text always returns empty regardless of max_len.
    #[test]
    fn redact_empty_always_empty(max_len in 0usize..=100) {
        let result = redact_text_for_log("", max_len);
        prop_assert_eq!(result, "");
    }

    // Property 16: Result is deterministic — same input gives same output.
    #[test]
    fn redact_deterministic(
        text in ".{0,128}",
        max_len in 1usize..=128,
    ) {
        let r1 = redact_text_for_log(&text, max_len);
        let r2 = redact_text_for_log(&text, max_len);
        prop_assert_eq!(r1, r2);
    }
}

// ── WorkflowStepPolicySummary + ActionKind serde cross-validation ───────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    // Property 17: ActionKind survives roundtrip through WorkflowStepPolicySummary serialization.
    #[test]
    fn action_kind_survives_summary_roundtrip(action in arb_action_kind()) {
        let summary = WorkflowStepPolicySummary {
            decision: WorkflowStepPolicyDecision::Allow,
            action: Some(action),
            rule_id: None,
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json).unwrap();
        prop_assert_eq!(parsed.action, Some(action));
    }

    // Property 18: Decision + action combination always serializes and parses.
    #[test]
    fn decision_action_combination_roundtrips(
        decision in arb_policy_decision(),
        action in arb_action_kind(),
        rule_id in proptest::option::of("[a-z.]{1,20}"),
    ) {
        let summary = WorkflowStepPolicySummary {
            decision,
            action: Some(action),
            rule_id: rule_id.clone(),
            reason: None,
            summary: None,
            error: None,
            decision_context: None,
        };
        let json = serde_json::to_string(&summary).unwrap();
        let parsed = WorkflowStepPolicySummary::parse(&json).unwrap();
        prop_assert_eq!(parsed.decision, decision);
        prop_assert_eq!(parsed.action, Some(action));
        prop_assert_eq!(parsed.rule_id, rule_id);
    }
}
