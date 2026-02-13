//! Property-based tests for the policy module
//!
//! Tests invariants for ActionKind, ActorKind, PaneCapabilities, PolicyDecision,
//! RiskScore, RiskConfig, Redactor, is_command_candidate, RateLimiter, PolicyEngine,
//! evaluate_policy_rules, and InjectionResult.

use frankenterm_core::policy::*;
use proptest::prelude::*;

// ============================================================================
// Strategies
// ============================================================================

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
    ]
}

fn arb_actor_kind() -> impl Strategy<Value = ActorKind> {
    prop_oneof![
        Just(ActorKind::Human),
        Just(ActorKind::Robot),
        Just(ActorKind::Mcp),
        Just(ActorKind::Workflow),
    ]
}

fn arb_risk_category() -> impl Strategy<Value = RiskCategory> {
    prop_oneof![
        Just(RiskCategory::State),
        Just(RiskCategory::Action),
        Just(RiskCategory::Context),
        Just(RiskCategory::Content),
    ]
}

fn arb_pane_capabilities() -> impl Strategy<Value = PaneCapabilities> {
    (
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::option::of(proptest::bool::ANY),
        proptest::bool::ANY,
        proptest::bool::ANY,
        proptest::option::of("[a-z-]{3,20}"),
    )
        .prop_map(
            |(prompt_active, command_running, alt_screen, has_recent_gap, is_reserved, reserved_by)| {
                PaneCapabilities {
                    prompt_active,
                    command_running,
                    alt_screen,
                    has_recent_gap,
                    is_reserved,
                    reserved_by,
                }
            },
        )
}

fn arb_applied_risk_factor() -> impl Strategy<Value = AppliedRiskFactor> {
    ("[a-z._]{3,30}", 0..=100u8, "[a-zA-Z ]{5,50}").prop_map(|(id, weight, explanation)| {
        AppliedRiskFactor {
            id,
            weight,
            explanation,
        }
    })
}

fn arb_risk_score() -> impl Strategy<Value = RiskScore> {
    prop_oneof![
        // Zero risk
        Just(RiskScore::zero()),
        // From factors (1-5 factors)
        proptest::collection::vec(arb_applied_risk_factor(), 1..=5)
            .prop_map(RiskScore::from_factors),
    ]
}

fn arb_rule_evaluation() -> impl Strategy<Value = RuleEvaluation> {
    (
        "[a-z._]{3,30}",
        proptest::bool::ANY,
        proptest::option::of(prop_oneof![
            Just("allow".to_string()),
            Just("deny".to_string()),
            Just("require_approval".to_string()),
        ]),
        proptest::option::of("[a-zA-Z ]{5,30}"),
    )
        .prop_map(|(rule_id, matched, decision, reason)| RuleEvaluation {
            rule_id,
            matched,
            decision,
            reason,
        })
}

fn arb_decision_evidence() -> impl Strategy<Value = DecisionEvidence> {
    ("[a-z_]{3,20}", "[a-zA-Z0-9 ]{1,30}").prop_map(|(key, value)| DecisionEvidence {
        key,
        value,
    })
}

fn arb_rate_limit_snapshot() -> impl Strategy<Value = RateLimitSnapshot> {
    (
        prop_oneof![
            Just("global".to_string()),
            (0..1000u64).prop_map(|id| format!("per_pane:{}", id)),
        ],
        "[a-z_]{3,20}",
        1..1000u32,
        0..100usize,
        0..120u64,
    )
        .prop_map(|(scope, action, limit, current, retry_after_secs)| RateLimitSnapshot {
            scope,
            action,
            limit,
            current,
            retry_after_secs,
        })
}

fn arb_decision_context() -> impl Strategy<Value = DecisionContext> {
    (
        0..2_000_000_000_000i64,
        arb_action_kind(),
        arb_actor_kind(),
        proptest::option::of(0..1000u64),
        proptest::option::of("[a-z]{3,10}"),
        arb_pane_capabilities(),
        proptest::option::of("[a-z ]{3,30}"),
        proptest::option::of("[a-z-]{3,20}"),
        proptest::collection::vec(arb_rule_evaluation(), 0..3),
        proptest::option::of("[a-z._]{3,20}"),
        proptest::collection::vec(arb_decision_evidence(), 0..3),
        proptest::option::of(arb_rate_limit_snapshot()),
    )
        .prop_map(
            |(
                timestamp_ms,
                action,
                actor,
                pane_id,
                domain,
                capabilities,
                text_summary,
                workflow_id,
                rules_evaluated,
                determining_rule,
                evidence,
                rate_limit,
            )| {
                DecisionContext {
                    timestamp_ms,
                    action,
                    actor,
                    pane_id,
                    domain,
                    capabilities,
                    text_summary,
                    workflow_id,
                    rules_evaluated,
                    determining_rule,
                    evidence,
                    rate_limit,
                    risk: None, // Skip RiskScore in context to avoid nesting complexity
                }
            },
        )
}

fn arb_policy_decision() -> impl Strategy<Value = PolicyDecision> {
    prop_oneof![
        // Allow variants
        Just(PolicyDecision::allow()),
        "[a-z._]{3,20}".prop_map(|rule| PolicyDecision::allow_with_rule(rule)),
        // Deny variants
        "[a-zA-Z ]{5,50}".prop_map(|reason| PolicyDecision::deny(reason)),
        ("[a-zA-Z ]{5,50}", "[a-z._]{3,20}")
            .prop_map(|(reason, rule)| PolicyDecision::deny_with_rule(reason, rule)),
        // RequireApproval variants
        "[a-zA-Z ]{5,50}".prop_map(|reason| PolicyDecision::require_approval(reason)),
        ("[a-zA-Z ]{5,50}", "[a-z._]{3,20}")
            .prop_map(|(reason, rule)| PolicyDecision::require_approval_with_rule(reason, rule)),
    ]
}

// ============================================================================
// ActionKind Properties
// ============================================================================

proptest! {
    /// Property 1: ActionKind serde roundtrip
    #[test]
    fn prop_action_kind_serde_roundtrip(action in arb_action_kind()) {
        let json = serde_json::to_string(&action).unwrap();
        let back: ActionKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(action, back);
    }

    /// Property 2: ActionKind as_str is snake_case (lowercase with underscores)
    #[test]
    fn prop_action_kind_as_str_snake_case(action in arb_action_kind()) {
        let s = action.as_str();
        prop_assert!(!s.is_empty(), "as_str should not be empty");
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "as_str should be snake_case: {}", s
        );
    }

    /// Property 3: ActionKind serde serialization matches as_str
    #[test]
    fn prop_action_kind_serde_matches_as_str(action in arb_action_kind()) {
        let json = serde_json::to_string(&action).unwrap();
        let expected = format!("\"{}\"", action.as_str());
        prop_assert_eq!(json, expected, "Serde and as_str should agree");
    }

    /// Property 4: is_destructive implies is_mutating or is non-pane destructive
    #[test]
    fn prop_action_kind_destructive_consistency(action in arb_action_kind()) {
        if action.is_destructive() {
            // DeleteFile is destructive but not mutating (it's a file op, not pane mutation)
            // Close, SendCtrlC, SendCtrlD are both destructive and mutating
            let is_file_op = matches!(action, ActionKind::DeleteFile);
            if !is_file_op {
                prop_assert!(action.is_mutating(),
                    "Destructive pane action {:?} should also be mutating", action);
            }
        }
    }

    /// Property 5: ReadOutput and SearchOutput are never mutating, destructive, or rate-limited
    #[test]
    fn prop_action_kind_read_actions_passive(action in arb_action_kind()) {
        if matches!(action, ActionKind::ReadOutput | ActionKind::SearchOutput) {
            prop_assert!(!action.is_mutating(), "Read actions should not be mutating");
            prop_assert!(!action.is_destructive(), "Read actions should not be destructive");
            prop_assert!(!action.is_rate_limited(), "Read actions should not be rate limited");
        }
    }

    // ========================================================================
    // ActorKind Properties
    // ========================================================================

    /// Property 6: ActorKind serde roundtrip
    #[test]
    fn prop_actor_kind_serde_roundtrip(actor in arb_actor_kind()) {
        let json = serde_json::to_string(&actor).unwrap();
        let back: ActorKind = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(actor, back);
    }

    /// Property 7: ActorKind as_str is snake_case
    #[test]
    fn prop_actor_kind_as_str_snake_case(actor in arb_actor_kind()) {
        let s = actor.as_str();
        prop_assert!(!s.is_empty(), "as_str should not be empty");
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "as_str should be snake_case: {}", s
        );
    }

    /// Property 8: Only Human is trusted
    #[test]
    fn prop_actor_kind_trust(actor in arb_actor_kind()) {
        if matches!(actor, ActorKind::Human) {
            prop_assert!(actor.is_trusted(), "Human should be trusted");
        } else {
            prop_assert!(!actor.is_trusted(), "{:?} should not be trusted", actor);
        }
    }

    // ========================================================================
    // PaneCapabilities Properties
    // ========================================================================

    /// Property 9: PaneCapabilities serde roundtrip
    #[test]
    fn prop_pane_capabilities_serde_roundtrip(caps in arb_pane_capabilities()) {
        let json = serde_json::to_string(&caps).unwrap();
        let back: PaneCapabilities = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(caps, back);
    }

    /// Property 10: is_state_known iff alt_screen is Some
    #[test]
    fn prop_pane_capabilities_state_known(caps in arb_pane_capabilities()) {
        prop_assert_eq!(caps.is_state_known(), caps.alt_screen.is_some(),
            "is_state_known should match alt_screen.is_some()");
    }

    /// Property 11: is_input_safe requires all safety conditions
    #[test]
    fn prop_pane_capabilities_input_safe(caps in arb_pane_capabilities()) {
        let expected = caps.prompt_active
            && !caps.command_running
            && caps.alt_screen == Some(false)
            && !caps.has_recent_gap
            && !caps.is_reserved;
        prop_assert_eq!(caps.is_input_safe(), expected,
            "is_input_safe should match conjunction of all safety conditions");
    }

    /// Property 12: clear_gap_on_prompt only clears if prompt_active
    #[test]
    fn prop_pane_capabilities_clear_gap_requires_prompt(
        mut caps in arb_pane_capabilities(),
    ) {
        caps.has_recent_gap = true;
        let was_prompt_active = caps.prompt_active;
        caps.clear_gap_on_prompt();
        if was_prompt_active {
            prop_assert!(!caps.has_recent_gap, "Gap should be cleared when prompt active");
        } else {
            prop_assert!(caps.has_recent_gap, "Gap should NOT be cleared without prompt");
        }
    }

    /// Property 13: PaneCapabilities::prompt() is input safe
    #[test]
    fn prop_pane_capabilities_prompt_safe(_dummy in Just(())) {
        let caps = PaneCapabilities::prompt();
        prop_assert!(caps.is_input_safe(), "prompt() should be input safe");
        prop_assert!(caps.prompt_active);
        prop_assert!(!caps.command_running);
        prop_assert_eq!(caps.alt_screen, Some(false));
    }

    /// Property 14: PaneCapabilities::running() is NOT input safe
    #[test]
    fn prop_pane_capabilities_running_not_safe(_dummy in Just(())) {
        let caps = PaneCapabilities::running();
        prop_assert!(!caps.is_input_safe(), "running() should not be input safe");
        prop_assert!(caps.command_running);
    }

    /// Property 15: PaneCapabilities::unknown() is NOT input safe, NOT state known
    #[test]
    fn prop_pane_capabilities_unknown_not_safe(_dummy in Just(())) {
        let caps = PaneCapabilities::unknown();
        prop_assert!(!caps.is_input_safe(), "unknown() should not be input safe");
        prop_assert!(!caps.is_state_known(), "unknown() should not be state known");
    }

    /// Property 16: PaneCapabilities::alt_screen() is NOT input safe
    #[test]
    fn prop_pane_capabilities_alt_screen_not_safe(_dummy in Just(())) {
        let caps = PaneCapabilities::alt_screen();
        prop_assert!(!caps.is_input_safe(), "alt_screen() should not be input safe");
        prop_assert_eq!(caps.alt_screen, Some(true));
    }

    // ========================================================================
    // RiskCategory Properties
    // ========================================================================

    /// Property 17: RiskCategory serde roundtrip
    #[test]
    fn prop_risk_category_serde_roundtrip(cat in arb_risk_category()) {
        let json = serde_json::to_string(&cat).unwrap();
        let back: RiskCategory = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(cat, back);
    }

    /// Property 18: RiskCategory as_str is snake_case
    #[test]
    fn prop_risk_category_as_str_snake_case(cat in arb_risk_category()) {
        let s = cat.as_str();
        prop_assert!(
            s.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "as_str should be snake_case: {}", s
        );
    }

    // ========================================================================
    // RiskScore Properties
    // ========================================================================

    /// Property 19: RiskScore from_factors capped at 100
    #[test]
    fn prop_risk_score_capped_at_100(
        factors in proptest::collection::vec(arb_applied_risk_factor(), 0..=10),
    ) {
        let score = RiskScore::from_factors(factors);
        prop_assert!(score.score <= 100, "Score {} should be <= 100", score.score);
    }

    /// Property 20: RiskScore zero is low risk
    #[test]
    fn prop_risk_score_zero_is_low(_dummy in Just(())) {
        let score = RiskScore::zero();
        prop_assert_eq!(score.score, 0);
        prop_assert!(score.is_low());
        prop_assert!(!score.is_medium());
        prop_assert!(!score.is_elevated());
        prop_assert!(!score.is_high());
    }

    /// Property 21: RiskScore tier methods are mutually exclusive and cover all scores
    #[test]
    fn prop_risk_score_tier_exclusive(score_val in 0..=100u8) {
        let score = RiskScore {
            score: score_val,
            factors: Vec::new(),
            summary: String::new(),
        };
        let tiers = [score.is_low(), score.is_medium(), score.is_elevated(), score.is_high()];
        let active_count = tiers.iter().filter(|&&t| t).count();
        prop_assert_eq!(active_count, 1,
            "Exactly one tier should be active for score {}, got {}",
            score_val, active_count);
    }

    /// Property 22: RiskScore summary_for_score matches tier boundaries
    #[test]
    fn prop_risk_score_summary_matches_tier(score_val in 0..=100u8) {
        let summary = RiskScore::summary_for_score(score_val);
        match score_val {
            0..=20 => prop_assert_eq!(summary, "Low risk"),
            21..=50 => prop_assert_eq!(summary, "Medium risk"),
            51..=70 => prop_assert_eq!(summary, "Elevated risk"),
            71..=100 => prop_assert_eq!(summary, "High risk"),
            _ => unreachable!(),
        }
    }

    /// Property 23: RiskScore serde roundtrip
    #[test]
    fn prop_risk_score_serde_roundtrip(score in arb_risk_score()) {
        let json = serde_json::to_string(&score).unwrap();
        let back: RiskScore = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(score.score, back.score);
        prop_assert_eq!(score.factors.len(), back.factors.len());
        prop_assert_eq!(&score.summary, &back.summary);
    }

    // ========================================================================
    // RiskConfig Properties
    // ========================================================================

    /// Property 24: RiskConfig get_weight returns 0 for disabled factors
    #[test]
    fn prop_risk_config_disabled_returns_zero(
        factor_id in "[a-z._]{3,20}",
        base_weight in 0..=100u8,
    ) {
        let mut config = RiskConfig::default();
        config.disabled.insert(factor_id.clone());
        let weight = config.get_weight(&factor_id, base_weight);
        prop_assert_eq!(weight, 0, "Disabled factor should have weight 0");
    }

    /// Property 25: RiskConfig get_weight uses override when present
    #[test]
    fn prop_risk_config_weight_override(
        factor_id in "[a-z._]{3,20}",
        base_weight in 0..=100u8,
        override_weight in 0..=100u8,
    ) {
        let mut config = RiskConfig::default();
        config.weights.insert(factor_id.clone(), override_weight);
        let weight = config.get_weight(&factor_id, base_weight);
        prop_assert_eq!(weight, override_weight.min(100),
            "Weight should use override (capped at 100)");
    }

    /// Property 26: RiskConfig get_weight falls back to base when no override
    #[test]
    fn prop_risk_config_weight_fallback(
        factor_id in "[a-z._]{3,20}",
        base_weight in 0..=100u8,
    ) {
        let config = RiskConfig::default();
        let weight = config.get_weight(&factor_id, base_weight);
        prop_assert_eq!(weight, base_weight.min(100),
            "Weight should fall back to base (capped at 100)");
    }

    /// Property 27: RiskConfig get_weight always <= 100
    #[test]
    fn prop_risk_config_weight_capped(
        factor_id in "[a-z._]{3,20}",
        base_weight in 0..=255u8, // u8 max
    ) {
        let config = RiskConfig::default();
        let weight = config.get_weight(&factor_id, base_weight);
        prop_assert!(weight <= 100, "Weight {} should be <= 100", weight);
    }

    /// Property 28: RiskConfig serde roundtrip
    #[test]
    fn prop_risk_config_serde_roundtrip(
        enabled in proptest::bool::ANY,
        allow_max in 0..=100u8,
        require_approval_max in 0..=100u8,
    ) {
        let config = RiskConfig {
            enabled,
            allow_max,
            require_approval_max,
            weights: std::collections::HashMap::new(),
            disabled: std::collections::HashSet::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: RiskConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(config.enabled, back.enabled);
        prop_assert_eq!(config.allow_max, back.allow_max);
        prop_assert_eq!(config.require_approval_max, back.require_approval_max);
    }

    // ========================================================================
    // RiskFactor Properties
    // ========================================================================

    /// Property 29: RiskFactor base_weight capped at 100
    #[test]
    fn prop_risk_factor_weight_capped(
        weight in 0..=255u8,
    ) {
        let factor = RiskFactor::new("test.factor", RiskCategory::State, weight, "desc");
        prop_assert!(factor.base_weight <= 100,
            "base_weight {} should be capped at 100", factor.base_weight);
    }

    // ========================================================================
    // PolicyDecision Properties
    // ========================================================================

    /// Property 30: PolicyDecision serde roundtrip
    #[test]
    fn prop_policy_decision_serde_roundtrip(decision in arb_policy_decision()) {
        let json = serde_json::to_string(&decision).unwrap();
        let back: PolicyDecision = serde_json::from_str(&json).unwrap();
        // Check variant preservation
        prop_assert_eq!(decision.is_allowed(), back.is_allowed());
        prop_assert_eq!(decision.is_denied(), back.is_denied());
        prop_assert_eq!(decision.requires_approval(), back.requires_approval());
        prop_assert_eq!(decision.as_str(), back.as_str());
    }

    /// Property 31: PolicyDecision variant checks are mutually exclusive
    #[test]
    fn prop_policy_decision_variant_exclusive(decision in arb_policy_decision()) {
        let checks = [
            decision.is_allowed(),
            decision.is_denied(),
            decision.requires_approval(),
        ];
        let active = checks.iter().filter(|&&v| v).count();
        prop_assert_eq!(active, 1,
            "Exactly one variant check should be true, got {}", active);
    }

    /// Property 32: PolicyDecision::deny cannot be overridden by with_approval
    #[test]
    fn prop_policy_decision_deny_resists_approval(
        reason in "[a-zA-Z ]{5,50}",
    ) {
        let deny = PolicyDecision::deny(&reason);
        let approval = ApprovalRequest {
            allow_once_code: "ABCD1234".to_string(),
            allow_once_full_hash: "sha256:fake".to_string(),
            expires_at: 999_999_999_999,
            summary: "bypass attempt".to_string(),
            command: "ft approve ABCD1234".to_string(),
        };
        let after = deny.with_approval(approval);
        prop_assert!(after.is_denied(), "Deny should not be overridden by with_approval");
        prop_assert!(!after.requires_approval());
        prop_assert!(!after.is_allowed());
    }

    /// Property 33: PolicyDecision::allow is unchanged by with_approval
    #[test]
    fn prop_policy_decision_allow_ignores_approval(_dummy in Just(())) {
        let allow = PolicyDecision::allow();
        let approval = ApprovalRequest {
            allow_once_code: "TEST".to_string(),
            allow_once_full_hash: "sha256:test".to_string(),
            expires_at: 0,
            summary: "ignored".to_string(),
            command: "ft approve TEST".to_string(),
        };
        let after = allow.with_approval(approval);
        prop_assert!(after.is_allowed(), "Allow should remain after with_approval");
        prop_assert!(after.approval_request().is_none());
    }

    /// Property 34: PolicyDecision as_str matches variant
    #[test]
    fn prop_policy_decision_as_str_matches(decision in arb_policy_decision()) {
        match decision.as_str() {
            "allow" => prop_assert!(decision.is_allowed()),
            "deny" => prop_assert!(decision.is_denied()),
            "require_approval" => prop_assert!(decision.requires_approval()),
            other => prop_assert!(false, "Unknown as_str: {}", other),
        }
    }

    /// Property 35: PolicyDecision reason is Some for Deny and RequireApproval, None for Allow
    #[test]
    fn prop_policy_decision_reason(decision in arb_policy_decision()) {
        match &decision {
            PolicyDecision::Allow { .. } => {
                prop_assert!(decision.reason().is_none(),
                    "Allow should have no reason");
            }
            PolicyDecision::Deny { reason, .. } => {
                prop_assert_eq!(decision.reason(), Some(reason.as_str()),
                    "Deny reason should match");
            }
            PolicyDecision::RequireApproval { reason, .. } => {
                prop_assert_eq!(decision.reason(), Some(reason.as_str()),
                    "RequireApproval reason should match");
            }
        }
    }

    /// Property 36: with_context preserves decision variant and attaches context
    #[test]
    fn prop_policy_decision_with_context(
        decision in arb_policy_decision(),
        ctx in arb_decision_context(),
    ) {
        let original_str = decision.as_str().to_string();
        let with_ctx = decision.with_context(ctx);
        prop_assert_eq!(with_ctx.as_str(), original_str.as_str(),
            "Decision variant should be preserved after with_context");
        prop_assert!(with_ctx.context().is_some(),
            "Context should be attached after with_context");
    }

    // ========================================================================
    // DecisionContext Properties
    // ========================================================================

    /// Property 37: DecisionContext serde roundtrip
    #[test]
    fn prop_decision_context_serde_roundtrip(ctx in arb_decision_context()) {
        let json = serde_json::to_string(&ctx).unwrap();
        let back: DecisionContext = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(ctx.timestamp_ms, back.timestamp_ms);
        prop_assert_eq!(ctx.action, back.action);
        prop_assert_eq!(ctx.actor, back.actor);
        prop_assert_eq!(ctx.pane_id, back.pane_id);
        prop_assert_eq!(&ctx.domain, &back.domain);
        prop_assert_eq!(ctx.capabilities, back.capabilities);
        prop_assert_eq!(&ctx.text_summary, &back.text_summary);
        prop_assert_eq!(&ctx.workflow_id, &back.workflow_id);
        prop_assert_eq!(ctx.rules_evaluated.len(), back.rules_evaluated.len());
        prop_assert_eq!(&ctx.determining_rule, &back.determining_rule);
        prop_assert_eq!(ctx.evidence.len(), back.evidence.len());
    }

    /// Property 38: DecisionContext::empty has sensible defaults
    #[test]
    fn prop_decision_context_empty(_dummy in Just(())) {
        let ctx = DecisionContext::empty();
        prop_assert_eq!(ctx.timestamp_ms, 0);
        prop_assert!(ctx.rules_evaluated.is_empty());
        prop_assert!(ctx.determining_rule.is_none());
        prop_assert!(ctx.evidence.is_empty());
        prop_assert!(ctx.rate_limit.is_none());
        prop_assert!(ctx.risk.is_none());
    }

    /// Property 39: record_rule appends to rules_evaluated
    #[test]
    fn prop_decision_context_record_rule(
        n_rules in 1..10usize,
    ) {
        let mut ctx = DecisionContext::empty();
        for i in 0..n_rules {
            ctx.record_rule(format!("rule.{}", i), i % 2 == 0, None, None);
        }
        prop_assert_eq!(ctx.rules_evaluated.len(), n_rules,
            "Should have {} rules, got {}", n_rules, ctx.rules_evaluated.len());
    }

    /// Property 40: add_evidence appends to evidence
    #[test]
    fn prop_decision_context_add_evidence(
        n_items in 1..10usize,
    ) {
        let mut ctx = DecisionContext::empty();
        for i in 0..n_items {
            ctx.add_evidence(format!("key_{}", i), format!("val_{}", i));
        }
        prop_assert_eq!(ctx.evidence.len(), n_items);
    }

    // ========================================================================
    // is_command_candidate Properties
    // ========================================================================

    /// Property 41: Empty string is never a command candidate
    #[test]
    fn prop_command_candidate_empty(_dummy in Just(())) {
        prop_assert!(!is_command_candidate(""), "Empty string should not be a command");
        prop_assert!(!is_command_candidate("  "), "Whitespace should not be a command");
        prop_assert!(!is_command_candidate("\n\n"), "Newlines should not be a command");
    }

    /// Property 42: Comments (# prefix) are never command candidates
    #[test]
    fn prop_command_candidate_comments(text in "[a-zA-Z0-9 ]{1,50}") {
        let comment = format!("# {}", text);
        prop_assert!(!is_command_candidate(&comment),
            "Comment should not be a command: {}", comment);
    }

    /// Property 43: Known command tokens are recognized
    #[test]
    fn prop_command_candidate_known_tokens(
        token in prop_oneof![
            Just("git"), Just("rm"), Just("sudo"), Just("docker"),
            Just("cargo"), Just("npm"), Just("python"), Just("node"),
        ],
        args in "[a-z ]{0,20}",
    ) {
        let cmd = format!("{} {}", token, args);
        prop_assert!(is_command_candidate(&cmd),
            "Known token '{}' should be a command candidate: {}", token, cmd);
    }

    /// Property 44: Dollar-prefixed commands are recognized
    #[test]
    fn prop_command_candidate_dollar_prefix(
        token in prop_oneof![
            Just("git"), Just("rm"), Just("cargo"), Just("npm"),
        ],
        args in "[a-z ]{0,20}",
    ) {
        let cmd = format!("$ {} {}", token, args);
        prop_assert!(is_command_candidate(&cmd),
            "Dollar-prefixed '{}' should be a command: {}", token, cmd);
    }

    /// Property 45: Shell operators make text a command candidate
    #[test]
    fn prop_command_candidate_operators(
        left in "[a-z]{3,10}",
        op in prop_oneof![Just("&&"), Just("||"), Just("|"), Just(">"), Just(";")],
        right in "[a-z]{3,10}",
    ) {
        let cmd = format!("{} {} {}", left, op, right);
        prop_assert!(is_command_candidate(&cmd),
            "Shell operator '{}' should make it a command: {}", op, cmd);
    }

    // ========================================================================
    // Redactor Properties
    // ========================================================================

    /// Property 46: Redactor idempotency - redacting twice gives same result
    #[test]
    fn prop_redactor_idempotent(text in "[a-zA-Z0-9 =:_-]{0,200}") {
        let redactor = Redactor::new();
        let once = redactor.redact(&text);
        let twice = redactor.redact(&once);
        prop_assert_eq!(&once, &twice, "Redacting twice should give same result");
    }

    /// Property 47: contains_secrets and redact are consistent
    #[test]
    fn prop_redactor_contains_vs_redact(text in "[a-zA-Z0-9 =:_-]{0,200}") {
        let redactor = Redactor::new();
        let has_secrets = redactor.contains_secrets(&text);
        let redacted = redactor.redact(&text);
        if has_secrets {
            prop_assert!(redacted.contains("[REDACTED]"),
                "If contains_secrets, redacted text should have [REDACTED]");
        }
        if !redacted.contains("[REDACTED]") {
            // If no redaction markers, either no secrets or text didn't change
            prop_assert_eq!(&redacted, &text,
                "No [REDACTED] markers means text unchanged");
        }
    }

    /// Property 48: debug markers include pattern names
    #[test]
    fn prop_redactor_debug_markers(_dummy in Just(())) {
        let redactor = Redactor::with_debug_markers();
        let input = "sk-abc123456789012345678901234567890123456789012345678901";
        let output = redactor.redact(input);
        prop_assert!(output.contains("[REDACTED:"), "Debug markers should include pattern name");
    }

    /// Property 49: detect returns sorted positions
    #[test]
    fn prop_redactor_detect_sorted(text in "[a-zA-Z0-9 =:_-]{0,200}") {
        let redactor = Redactor::new();
        let detections = redactor.detect(&text);
        for window in detections.windows(2) {
            prop_assert!(window[0].1 <= window[1].1,
                "Detections should be sorted by start position");
        }
    }

    // ========================================================================
    // PolicyEngine Properties
    // ========================================================================

    /// Property 50: Human actors bypass alt-screen unknown check
    #[test]
    fn prop_engine_human_bypasses_alt_unknown(
        action in prop_oneof![Just(ActionKind::SendText), Just(ActionKind::SendControl)],
    ) {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.alt_screen = None; // Unknown
        let input = PolicyInput::new(action, ActorKind::Human)
            .with_pane(1)
            .with_capabilities(caps);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(),
            "Human should be allowed with unknown alt-screen");
    }

    /// Property 51: Alt-screen active always denies SendText/SendControl (even human)
    #[test]
    fn prop_engine_alt_screen_always_denies(
        actor in arb_actor_kind(),
        action in prop_oneof![Just(ActionKind::SendText), Just(ActionKind::SendControl)],
    ) {
        let mut engine = PolicyEngine::permissive();
        let caps = PaneCapabilities::alt_screen();
        let input = PolicyInput::new(action, actor)
            .with_pane(1)
            .with_capabilities(caps);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(),
            "Alt-screen should deny {:?} for {:?}", action, actor);
        prop_assert_eq!(decision.rule_id(), Some("policy.alt_screen"));
    }

    /// Property 52: Read/search actions always allowed regardless of pane state
    #[test]
    fn prop_engine_read_always_allowed(
        actor in arb_actor_kind(),
        caps in arb_pane_capabilities(),
    ) {
        let mut engine = PolicyEngine::strict();
        let input = PolicyInput::new(ActionKind::ReadOutput, actor)
            .with_pane(1)
            .with_capabilities(caps);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(),
            "ReadOutput should always be allowed");
    }

    /// Property 53: Destructive actions by non-human require approval (when not in alt-screen)
    #[test]
    fn prop_engine_destructive_non_human_needs_approval(
        actor in prop_oneof![Just(ActorKind::Robot), Just(ActorKind::Mcp), Just(ActorKind::Workflow)],
    ) {
        let mut engine = PolicyEngine::permissive();
        // Close is destructive but not a send action, so alt-screen/prompt checks don't apply
        let input = PolicyInput::new(ActionKind::Close, actor).with_pane(1);
        let decision = engine.authorize(&input);
        prop_assert!(decision.requires_approval(),
            "Destructive action by {:?} should require approval", actor);
        prop_assert_eq!(decision.rule_id(), Some("policy.destructive_action"));
    }

    /// Property 54: Destructive actions by human are allowed
    #[test]
    fn prop_engine_destructive_human_allowed(_dummy in Just(())) {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::Close, ActorKind::Human).with_pane(1);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(),
            "Human destructive actions should be allowed");
    }

    /// Property 55: Reserved pane denies mutating actions from other workflows
    #[test]
    fn prop_engine_reserved_pane_denies_other(
        my_wf in "[a-z]{3,10}",
        other_wf in "[a-z]{3,10}",
    ) {
        prop_assume!(my_wf != other_wf);
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = true;
        caps.reserved_by = Some(other_wf.clone());
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(1)
            .with_capabilities(caps)
            .with_workflow(&my_wf);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(),
            "Reserved pane should deny other workflow");
        prop_assert_eq!(decision.rule_id(), Some("policy.pane_reserved"));
    }

    /// Property 56: Reserved pane allows owning workflow
    #[test]
    fn prop_engine_reserved_pane_allows_owner(wf_id in "[a-z]{3,10}") {
        let mut engine = PolicyEngine::permissive();
        let mut caps = PaneCapabilities::prompt();
        caps.is_reserved = true;
        caps.reserved_by = Some(wf_id.clone());
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Workflow)
            .with_pane(1)
            .with_capabilities(caps)
            .with_workflow(&wf_id);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(),
            "Reserved pane should allow owning workflow");
    }

    /// Property 57: Risk scoring disabled returns zero
    #[test]
    fn prop_engine_risk_disabled(
        action in arb_action_kind(),
        actor in arb_actor_kind(),
    ) {
        let engine = PolicyEngine::permissive()
            .with_risk_config(RiskConfig { enabled: false, ..Default::default() });
        let input = PolicyInput::new(action, actor);
        let risk = engine.calculate_risk(&input);
        prop_assert_eq!(risk.score, 0, "Disabled risk scoring should return 0");
    }

    /// Property 58: risk_to_decision respects thresholds
    #[test]
    fn prop_engine_risk_to_decision_thresholds(score_val in 0..=100u8) {
        let engine = PolicyEngine::permissive(); // default: allow_max=50, require_approval_max=70
        let risk = RiskScore {
            score: score_val,
            factors: Vec::new(),
            summary: String::new(),
        };
        let input = PolicyInput::new(ActionKind::ReadOutput, ActorKind::Robot);
        let decision = engine.risk_to_decision(&risk, &input);
        if score_val <= 50 {
            prop_assert!(decision.is_allowed(),
                "Score {} <= 50 should allow", score_val);
        } else if score_val <= 70 {
            prop_assert!(decision.requires_approval(),
                "Score {} in 51-70 should require approval", score_val);
        } else {
            prop_assert!(decision.is_denied(),
                "Score {} > 70 should deny", score_val);
        }
    }

    // ========================================================================
    // InjectionResult Properties
    // ========================================================================

    /// Property 59: InjectionResult variant checks are mutually exclusive
    #[test]
    fn prop_injection_result_variant_exclusive(
        variant in 0..4u32,
    ) {
        let result = match variant {
            0 => InjectionResult::Allowed {
                decision: PolicyDecision::allow(),
                summary: "test".to_string(),
                pane_id: 1,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
            1 => InjectionResult::Denied {
                decision: PolicyDecision::deny("reason"),
                summary: "test".to_string(),
                pane_id: 1,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
            2 => InjectionResult::RequiresApproval {
                decision: PolicyDecision::require_approval("reason"),
                summary: "test".to_string(),
                pane_id: 1,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
            _ => InjectionResult::Error {
                error: "err".to_string(),
                pane_id: 1,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
        };

        let checks = [
            result.is_allowed(),
            result.is_denied(),
            result.requires_approval(),
            result.error_message().is_some(),
        ];
        let active = checks.iter().filter(|&&v| v).count();
        prop_assert_eq!(active, 1,
            "Exactly one variant check should be true for variant {}", variant);
    }

    /// Property 60: InjectionResult serde roundtrip preserves variant
    #[test]
    fn prop_injection_result_serde_roundtrip(variant in 0..4u32) {
        let result = match variant {
            0 => InjectionResult::Allowed {
                decision: PolicyDecision::allow(),
                summary: "ls -la".to_string(),
                pane_id: 42,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
            1 => InjectionResult::Denied {
                decision: PolicyDecision::deny("blocked"),
                summary: "rm -rf".to_string(),
                pane_id: 1,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
            2 => InjectionResult::RequiresApproval {
                decision: PolicyDecision::require_approval("needs approval"),
                summary: "git reset".to_string(),
                pane_id: 5,
                action: ActionKind::SendCtrlC,
                audit_action_id: None,
            },
            _ => InjectionResult::Error {
                error: "connection lost".to_string(),
                pane_id: 99,
                action: ActionKind::SendText,
                audit_action_id: None,
            },
        };

        let json = serde_json::to_string(&result).unwrap();
        let back: InjectionResult = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(result.is_allowed(), back.is_allowed());
        prop_assert_eq!(result.is_denied(), back.is_denied());
        prop_assert_eq!(result.requires_approval(), back.requires_approval());
        prop_assert_eq!(result.error_message().is_some(), back.error_message().is_some());
    }

    /// Property 61: InjectionResult set_audit_action_id is retrievable
    #[test]
    fn prop_injection_result_audit_id(id in 1..10000i64) {
        let mut result = InjectionResult::Allowed {
            decision: PolicyDecision::allow(),
            summary: "test".to_string(),
            pane_id: 1,
            action: ActionKind::SendText,
            audit_action_id: None,
        };
        prop_assert!(result.audit_action_id().is_none());
        result.set_audit_action_id(id);
        prop_assert_eq!(result.audit_action_id(), Some(id));
    }

    // ========================================================================
    // PolicyInput Properties
    // ========================================================================

    /// Property 62: PolicyInput builder preserves fields
    #[test]
    fn prop_policy_input_builder(
        action in arb_action_kind(),
        actor in arb_actor_kind(),
        pane_id in 0..1000u64,
        domain in "[a-z]{3,10}",
    ) {
        let input = PolicyInput::new(action, actor)
            .with_pane(pane_id)
            .with_domain(&domain);
        prop_assert_eq!(input.action, action);
        prop_assert_eq!(input.actor, actor);
        prop_assert_eq!(input.pane_id, Some(pane_id));
        prop_assert_eq!(input.domain.as_deref(), Some(domain.as_str()));
    }

    /// Property 63: PolicyInput serde roundtrip (excluding skip fields)
    #[test]
    fn prop_policy_input_serde_roundtrip(
        action in arb_action_kind(),
        actor in arb_actor_kind(),
        pane_id in proptest::option::of(0..1000u64),
        domain in proptest::option::of("[a-z]{3,10}"),
    ) {
        let mut input = PolicyInput::new(action, actor);
        if let Some(pid) = pane_id {
            input = input.with_pane(pid);
        }
        if let Some(ref d) = domain {
            input = input.with_domain(d);
        }
        let json = serde_json::to_string(&input).unwrap();
        let back: PolicyInput = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(input.action, back.action);
        prop_assert_eq!(input.actor, back.actor);
        prop_assert_eq!(input.pane_id, back.pane_id);
        prop_assert_eq!(&input.domain, &back.domain);
        // command_text is serde(skip), so it won't roundtrip
        prop_assert!(back.command_text.is_none());
    }

    // ========================================================================
    // RateLimiter Properties
    // ========================================================================

    /// Property 64: RateLimiter different panes are independent
    #[test]
    fn prop_rate_limiter_pane_isolation(
        pane_a in 1..100u64,
        pane_b in 100..200u64,
    ) {
        let mut limiter = RateLimiter::new(1, 1000);
        // Fill pane_a's limit
        assert!(limiter.check(ActionKind::SendText, Some(pane_a)).is_allowed());
        // pane_b should still be allowed
        let outcome = limiter.check(ActionKind::SendText, Some(pane_b));
        prop_assert!(outcome.is_allowed(),
            "Different pane {} should not be affected by pane {}", pane_b, pane_a);
    }

    /// Property 65: RateLimiter different actions are independent
    #[test]
    fn prop_rate_limiter_action_isolation(
        pane_id in 1..100u64,
    ) {
        let mut limiter = RateLimiter::new(1, 1000);
        assert!(limiter.check(ActionKind::SendText, Some(pane_id)).is_allowed());
        // Different action should still be allowed
        let outcome = limiter.check(ActionKind::SendCtrlC, Some(pane_id));
        prop_assert!(outcome.is_allowed(),
            "Different action should not be limited by SendText");
    }

    /// Property 66: RateLimiter with limit 0 allows everything (disabled)
    #[test]
    fn prop_rate_limiter_zero_limit_allows(
        n in 1..20usize,
        pane_id in 1..100u64,
    ) {
        let mut limiter = RateLimiter::new(0, 0);
        for _ in 0..n {
            let outcome = limiter.check(ActionKind::SendText, Some(pane_id));
            prop_assert!(outcome.is_allowed(),
                "Zero limit should allow everything");
        }
    }

    // ========================================================================
    // Command Gate Properties
    // ========================================================================

    /// Property 67: rm -rf / always denied
    #[test]
    fn prop_command_gate_rm_rf_root_denied(_dummy in Just(())) {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("rm -rf /");
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(), "rm -rf / should be denied");
        prop_assert_eq!(decision.rule_id(), Some("command.rm_rf_root"));
    }

    /// Property 68: rm -rf ~ always denied
    #[test]
    fn prop_command_gate_rm_rf_home_denied(_dummy in Just(())) {
        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text("rm -rf ~");
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_denied(), "rm -rf ~ should be denied");
    }

    /// Property 69: Non-command text passes command gate
    #[test]
    fn prop_command_gate_non_command_passes(
        text in "[a-zA-Z ]{10,50}",
    ) {
        // Filter out texts that happen to contain command tokens
        prop_assume!(!text.to_lowercase().contains("git"));
        prop_assume!(!text.to_lowercase().contains("rm "));
        prop_assume!(!text.to_lowercase().contains("sudo"));
        prop_assume!(!text.to_lowercase().contains("npm"));
        prop_assume!(!text.to_lowercase().contains("cargo"));
        prop_assume!(!text.to_lowercase().contains("make"));
        prop_assume!(!text.to_lowercase().contains("node"));
        prop_assume!(!text.to_lowercase().contains("python"));
        prop_assume!(!text.to_lowercase().contains("bash"));
        prop_assume!(!text.to_lowercase().contains("find"));
        prop_assume!(!text.to_lowercase().contains("export"));
        prop_assume!(!text.contains("&&"));
        prop_assume!(!text.contains("||"));
        prop_assume!(!text.contains("|"));
        prop_assume!(!text.contains(">"));
        prop_assume!(!text.contains(";"));

        let mut engine = PolicyEngine::permissive();
        let input = PolicyInput::new(ActionKind::SendText, ActorKind::Robot)
            .with_pane(1)
            .with_capabilities(PaneCapabilities::prompt())
            .with_command_text(&text);
        let decision = engine.authorize(&input);
        prop_assert!(decision.is_allowed(),
            "Non-command text should be allowed: {}", text);
    }

    // ========================================================================
    // SendTextAuditSummary Properties
    // ========================================================================

    /// Property 70: build_send_text_audit_summary returns valid JSON
    #[test]
    fn prop_audit_summary_valid_json(
        text in "[a-zA-Z0-9 ]{1,100}",
        wf_id in proptest::option::of("[a-z-]{3,20}"),
    ) {
        let summary = build_send_text_audit_summary(&text, wf_id.as_deref(), None);
        let parsed: serde_json::Value = serde_json::from_str(&summary).unwrap();
        prop_assert!(parsed.is_object(), "Audit summary should be a JSON object");
        prop_assert!(parsed.get("text_length").is_some());
        prop_assert!(parsed.get("text_preview_redacted").is_some());
        prop_assert!(parsed.get("text_hash").is_some());
        prop_assert!(parsed.get("command_candidate").is_some());
    }

    /// Property 71: audit summary text_length matches input
    #[test]
    fn prop_audit_summary_length(text in "[a-zA-Z0-9 ]{1,100}") {
        let summary = build_send_text_audit_summary(&text, None, None);
        let parsed: SendTextAuditSummary = serde_json::from_str(&summary).unwrap();
        prop_assert_eq!(parsed.text_length, text.len(),
            "text_length should match input length");
    }

    /// Property 72: audit summary command_candidate matches is_command_candidate
    #[test]
    fn prop_audit_summary_command_candidate(text in "[a-zA-Z0-9 ]{1,100}") {
        let summary = build_send_text_audit_summary(&text, None, None);
        let parsed: SendTextAuditSummary = serde_json::from_str(&summary).unwrap();
        prop_assert_eq!(parsed.command_candidate, is_command_candidate(&text),
            "command_candidate should match is_command_candidate for: {}", text);
    }
}
