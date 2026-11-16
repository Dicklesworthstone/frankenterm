//! Property-based tests for policy rate-limit carrier types.
//!
//! Covers:
//! - `RateLimitScope`
//! - `RateLimitHit::reason`
//! - `RateLimitOutcome::is_allowed`
//! - `RuleEvaluationResult` clone/debug invariants

use std::time::Duration;

use frankenterm_core::config::{PolicyRule, PolicyRuleDecision, PolicyRuleMatch};
use frankenterm_core::policy::{
    ActionKind, RateLimitHit, RateLimitOutcome, RateLimitScope, RuleEvaluationResult,
};
use proptest::prelude::*;

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

fn arb_rate_limit_scope() -> impl Strategy<Value = RateLimitScope> {
    prop_oneof![
        Just(RateLimitScope::Global),
        (0u64..4096).prop_map(|pane_id| RateLimitScope::PerPane { pane_id }),
    ]
}

fn arb_rate_limit_hit() -> impl Strategy<Value = RateLimitHit> {
    (
        arb_rate_limit_scope(),
        arb_action_kind(),
        1u32..256,
        0usize..512,
        1u64..300,
        0u64..120,
    )
        .prop_map(
            |(scope, action, limit, current, window_secs, retry_after_secs)| RateLimitHit {
                scope,
                action,
                limit,
                current,
                window: Duration::from_secs(window_secs),
                retry_after: Duration::from_secs(retry_after_secs),
            },
        )
}

fn arb_policy_rule_decision() -> impl Strategy<Value = PolicyRuleDecision> {
    prop_oneof![
        Just(PolicyRuleDecision::Allow),
        Just(PolicyRuleDecision::Deny),
        Just(PolicyRuleDecision::RequireApproval),
    ]
}

fn arb_policy_rule_match() -> impl Strategy<Value = PolicyRuleMatch> {
    (
        proptest::collection::vec("[a-z_]{1,12}", 0..3),
        proptest::collection::vec("[a-z_]{1,12}", 0..3),
        proptest::collection::vec("[a-z_]{1,12}", 0..3),
        proptest::collection::vec(0u64..32, 0..3),
        proptest::collection::vec(".{1,16}", 0..2),
        proptest::collection::vec(".{1,16}", 0..2),
        proptest::collection::vec("[a-z:._-]{1,16}", 0..2),
        proptest::collection::vec(".{1,16}", 0..2),
        proptest::collection::vec("[a-z_]{1,12}", 0..2),
    )
        .prop_map(
            |(
                actions,
                actors,
                surfaces,
                pane_ids,
                pane_titles,
                pane_cwds,
                pane_domains,
                command_patterns,
                agent_types,
            )| PolicyRuleMatch {
                actions,
                actors,
                surfaces,
                pane_ids,
                pane_titles,
                pane_cwds,
                pane_domains,
                command_patterns,
                agent_types,
            },
        )
}

fn arb_policy_rule() -> impl Strategy<Value = PolicyRule> {
    (
        "[a-z0-9_.-]{1,32}",
        proptest::option::of(".{1,48}"),
        0u32..1000,
        arb_policy_rule_match(),
        arb_policy_rule_decision(),
        proptest::option::of(".{1,48}"),
    )
        .prop_map(
            |(id, description, priority, match_on, decision, message)| PolicyRule {
                id,
                description,
                priority,
                match_on,
                decision,
                message,
            },
        )
}

fn arb_rule_evaluation_result() -> impl Strategy<Value = RuleEvaluationResult> {
    (
        proptest::option::of(arb_policy_rule()),
        proptest::option::of(arb_policy_rule_decision()),
        proptest::collection::vec("[a-z0-9_.-]{1,24}", 0..8),
        proptest::collection::vec("[a-z0-9_.-]{1,24}", 0..8),
    )
        .prop_map(
            |(matching_rule, decision, rules_checked, matched_rule_ids)| RuleEvaluationResult {
                matching_rule,
                decision,
                rules_checked,
                matched_rule_ids,
            },
        )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    #[test]
    fn rate_limit_hit_reason_mentions_scope_and_action(hit in arb_rate_limit_hit()) {
        let reason = hit.reason();

        prop_assert!(reason.contains(hit.action.as_str()));
        prop_assert!(reason.contains("Remediation:"));

        match hit.scope {
            RateLimitScope::Global => {
                prop_assert!(reason.contains("Global rate limit exceeded"));
            }
            RateLimitScope::PerPane { pane_id } => {
                let pane_fragment = format!("pane {pane_id}");
                prop_assert!(reason.contains(&pane_fragment));
                prop_assert!(reason.contains("per-pane"));
            }
        }

        if hit.retry_after > Duration::ZERO {
            let retry_secs = hit.retry_after.as_millis().div_ceil(1000);
            let retry_fragment = format!("retry after {retry_secs}s");
            prop_assert!(reason.contains(&retry_fragment));
        } else {
            prop_assert!(!reason.contains("retry after"));
        }
    }

    #[test]
    fn rate_limit_outcome_is_allowed_matches_variant(hit in arb_rate_limit_hit()) {
        let allowed = RateLimitOutcome::Allowed;
        let limited = RateLimitOutcome::Limited(hit);

        prop_assert!(allowed.is_allowed());
        prop_assert!(!limited.is_allowed());
    }

    #[test]
    fn rule_evaluation_result_clone_preserves_fields(result in arb_rule_evaluation_result()) {
        let cloned = result.clone();

        prop_assert_eq!(cloned.decision, result.decision);
        prop_assert_eq!(cloned.rules_checked, result.rules_checked);
        prop_assert_eq!(cloned.matched_rule_ids, result.matched_rule_ids);
        prop_assert_eq!(
            cloned.matching_rule.as_ref().map(|rule| &rule.id),
            result.matching_rule.as_ref().map(|rule| &rule.id)
        );
    }

    #[test]
    fn rule_evaluation_result_debug_mentions_type_name(result in arb_rule_evaluation_result()) {
        let debug = format!("{result:?}");
        prop_assert!(debug.contains("RuleEvaluationResult"));
    }
}
