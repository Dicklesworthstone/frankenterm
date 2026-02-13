//! Property-based tests for approval (allow-once) token invariants.
//!
//! Bead: wa-0r1c
//!
//! Validates:
//! 1. fingerprint_for_input: deterministic (same input → same fingerprint)
//! 2. fingerprint_for_input: starts with "sha256:" prefix
//! 3. fingerprint_for_input: hex digest is 64 chars
//! 4. fingerprint_for_input: action_kind changes fingerprint
//! 5. fingerprint_for_input: pane_id changes fingerprint
//! 6. fingerprint_for_input: domain changes fingerprint
//! 7. fingerprint_for_input: text_summary changes fingerprint
//! 8. fingerprint_for_input: command_text changes fingerprint
//! 9. fingerprint_for_input: workflow_id changes fingerprint
//! 10. fingerprint_for_input: agent_type changes fingerprint
//! 11. fingerprint_for_input: pane_title changes fingerprint
//! 12. fingerprint_for_input: pane_cwd changes fingerprint
//! 13. hash_allow_once_code: deterministic
//! 14. hash_allow_once_code: starts with "sha256:" prefix
//! 15. hash_allow_once_code: hex digest is 64 chars
//! 16. hash_allow_once_code: different codes → different hashes
//! 17. ApprovalScope::from_input: workspace_id preserved
//! 18. ApprovalScope::from_input: action_kind matches
//! 19. ApprovalScope::from_input: pane_id matches
//! 20. ApprovalScope::from_input: fingerprint matches fingerprint_for_input
//! 21. ApprovalAuditContext::default: all fields None
//! 22. ApprovalConfig: serde roundtrip
//! 23. ApprovalConfig::default: sensible values

use proptest::prelude::*;

use frankenterm_core::approval::{
    ApprovalAuditContext, ApprovalScope, fingerprint_for_input, hash_allow_once_code,
};
use frankenterm_core::config::ApprovalConfig;
use frankenterm_core::policy::{ActionKind, ActorKind, PolicyInput};

// =============================================================================
// Strategies
// =============================================================================

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

fn arb_policy_input() -> impl Strategy<Value = PolicyInput> {
    (
        arb_action_kind(),
        arb_actor_kind(),
        proptest::option::of(1_u64..10000),
        proptest::option::of("[a-z]{3,10}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,30}"),
        proptest::option::of("[a-zA-Z0-9 ]{1,30}"),
        proptest::option::of("[a-zA-Z0-9_-]{1,20}"),
        proptest::option::of("[a-zA-Z0-9_]{1,15}"),
        proptest::option::of("[a-zA-Z0-9 /-]{1,30}"),
        proptest::option::of("[a-zA-Z0-9/]{1,30}"),
    )
        .prop_map(
            |(
                action,
                actor,
                pane_id,
                domain,
                text_summary,
                command_text,
                workflow_id,
                agent_type,
                pane_title,
                pane_cwd,
            )| {
                let mut input = PolicyInput::new(action, actor);
                if let Some(pid) = pane_id {
                    input = input.with_pane(pid);
                }
                if let Some(d) = domain {
                    input = input.with_domain(d);
                }
                if let Some(ts) = text_summary {
                    input = input.with_text_summary(ts);
                }
                if let Some(ct) = command_text {
                    input = input.with_command_text(ct);
                }
                if let Some(wid) = workflow_id {
                    input = input.with_workflow(wid);
                }
                if let Some(at) = agent_type {
                    input = input.with_agent_type(at);
                }
                if let Some(pt) = pane_title {
                    input = input.with_pane_title(pt);
                }
                if let Some(pc) = pane_cwd {
                    input = input.with_pane_cwd(pc);
                }
                input
            },
        )
}

// =============================================================================
// Property 1: fingerprint_for_input is deterministic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_deterministic(
        input in arb_policy_input(),
    ) {
        let fp1 = fingerprint_for_input(&input);
        let fp2 = fingerprint_for_input(&input);
        prop_assert_eq!(fp1, fp2);
    }
}

// =============================================================================
// Property 2: fingerprint starts with "sha256:"
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_sha256_prefix(
        input in arb_policy_input(),
    ) {
        let fp = fingerprint_for_input(&input);
        prop_assert!(fp.starts_with("sha256:"),
            "fingerprint should start with 'sha256:', got '{}'", fp);
    }
}

// =============================================================================
// Property 3: fingerprint hex digest is 64 chars
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_hex_length(
        input in arb_policy_input(),
    ) {
        let fp = fingerprint_for_input(&input);
        let hex = fp.strip_prefix("sha256:").unwrap();
        prop_assert_eq!(hex.len(), 64,
            "sha256 hex should be 64 chars, got {}", hex.len());
        prop_assert!(hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex should only contain hex digits, got '{}'", hex);
    }
}

// =============================================================================
// Property 4: different action_kind → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_action_kind_sensitivity(
        action1 in arb_action_kind(),
        action2 in arb_action_kind(),
        pane_id in proptest::option::of(1_u64..1000),
    ) {
        prop_assume!(action1.as_str() != action2.as_str());
        let mut i1 = PolicyInput::new(action1, ActorKind::Robot);
        let mut i2 = PolicyInput::new(action2, ActorKind::Robot);
        if let Some(pid) = pane_id {
            i1 = i1.with_pane(pid);
            i2 = i2.with_pane(pid);
        }
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 5: different pane_id → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_pane_id_sensitivity(
        pid1 in 1_u64..10000,
        pid2 in 1_u64..10000,
    ) {
        prop_assume!(pid1 != pid2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(pid1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane(pid2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 6: different domain → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_domain_sensitivity(
        d1 in "[a-z]{3,10}",
        d2 in "[a-z]{3,10}",
    ) {
        prop_assume!(d1 != d2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_domain(&d1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_domain(&d2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 7: different text_summary → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_text_summary_sensitivity(
        t1 in "[a-zA-Z0-9 ]{1,30}",
        t2 in "[a-zA-Z0-9 ]{1,30}",
    ) {
        prop_assume!(t1 != t2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_text_summary(&t1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_text_summary(&t2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 8: different command_text → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_command_text_sensitivity(
        c1 in "[a-zA-Z0-9 ]{1,30}",
        c2 in "[a-zA-Z0-9 ]{1,30}",
    ) {
        prop_assume!(c1 != c2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_command_text(&c1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_command_text(&c2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 9: different workflow_id → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_workflow_id_sensitivity(
        w1 in "[a-zA-Z0-9_-]{1,20}",
        w2 in "[a-zA-Z0-9_-]{1,20}",
    ) {
        prop_assume!(w1 != w2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_workflow(&w1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_workflow(&w2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 10: different agent_type → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_agent_type_sensitivity(
        a1 in "[a-z]{3,10}",
        a2 in "[a-z]{3,10}",
    ) {
        prop_assume!(a1 != a2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type(&a1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_agent_type(&a2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 11: different pane_title → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_pane_title_sensitivity(
        t1 in "[a-zA-Z0-9 ]{1,20}",
        t2 in "[a-zA-Z0-9 ]{1,20}",
    ) {
        prop_assume!(t1 != t2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_title(&t1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_title(&t2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 12: different pane_cwd → different fingerprint
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn fingerprint_pane_cwd_sensitivity(
        c1 in "[a-zA-Z0-9/]{1,30}",
        c2 in "[a-zA-Z0-9/]{1,30}",
    ) {
        prop_assume!(c1 != c2);
        let i1 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd(&c1);
        let i2 = PolicyInput::new(ActionKind::SendText, ActorKind::Robot).with_pane_cwd(&c2);
        prop_assert_ne!(fingerprint_for_input(&i1), fingerprint_for_input(&i2));
    }
}

// =============================================================================
// Property 13: hash_allow_once_code is deterministic
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn hash_code_deterministic(
        code in "[A-Z0-9]{4,16}",
    ) {
        let h1 = hash_allow_once_code(&code);
        let h2 = hash_allow_once_code(&code);
        prop_assert_eq!(h1, h2);
    }
}

// =============================================================================
// Property 14: hash_allow_once_code starts with "sha256:"
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn hash_code_sha256_prefix(
        code in "[A-Z0-9]{4,16}",
    ) {
        let h = hash_allow_once_code(&code);
        prop_assert!(h.starts_with("sha256:"),
            "hash should start with 'sha256:', got '{}'", h);
    }
}

// =============================================================================
// Property 15: hash_allow_once_code hex digest is 64 chars
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn hash_code_hex_length(
        code in "[A-Z0-9]{4,16}",
    ) {
        let h = hash_allow_once_code(&code);
        let hex = h.strip_prefix("sha256:").unwrap();
        prop_assert_eq!(hex.len(), 64,
            "sha256 hex should be 64 chars, got {}", hex.len());
        prop_assert!(hex.chars().all(|c| c.is_ascii_hexdigit()),
            "hex should only contain hex digits");
    }
}

// =============================================================================
// Property 16: different codes → different hashes
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn hash_code_collision_resistance(
        code1 in "[A-Z0-9]{4,16}",
        code2 in "[A-Z0-9]{4,16}",
    ) {
        prop_assume!(code1 != code2);
        prop_assert_ne!(hash_allow_once_code(&code1), hash_allow_once_code(&code2));
    }
}

// =============================================================================
// Property 17: ApprovalScope::from_input preserves workspace_id
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scope_workspace_id_preserved(
        ws in "[a-z]{3,15}",
        input in arb_policy_input(),
    ) {
        let scope = ApprovalScope::from_input(&ws, &input);
        prop_assert_eq!(scope.workspace_id, ws);
    }
}

// =============================================================================
// Property 18: ApprovalScope::from_input action_kind matches
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scope_action_kind_matches(
        input in arb_policy_input(),
    ) {
        let scope = ApprovalScope::from_input("ws", &input);
        prop_assert_eq!(scope.action_kind, input.action.as_str());
    }
}

// =============================================================================
// Property 19: ApprovalScope::from_input pane_id matches
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scope_pane_id_matches(
        input in arb_policy_input(),
    ) {
        let scope = ApprovalScope::from_input("ws", &input);
        prop_assert_eq!(scope.pane_id, input.pane_id);
    }
}

// =============================================================================
// Property 20: ApprovalScope::from_input fingerprint matches fingerprint_for_input
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    #[test]
    fn scope_fingerprint_matches(
        input in arb_policy_input(),
    ) {
        let scope = ApprovalScope::from_input("ws", &input);
        let expected = fingerprint_for_input(&input);
        prop_assert_eq!(scope.action_fingerprint, expected);
    }
}

// =============================================================================
// Property 21: ApprovalAuditContext::default all None
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn audit_context_default_all_none(_dummy in 0..1_u32) {
        let ctx = ApprovalAuditContext::default();
        prop_assert!(ctx.correlation_id.is_none());
        prop_assert!(ctx.decision_context.is_none());
    }
}

// =============================================================================
// Property 22: ApprovalConfig serde roundtrip
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn config_serde_roundtrip(
        expiry_secs in 0_u64..1_000_000,
        max_tokens in 1_u32..10000,
        reapproval in proptest::bool::ANY,
    ) {
        let config = ApprovalConfig {
            token_expiry_secs: expiry_secs,
            max_active_tokens: max_tokens,
            require_reapproval_on_failure: reapproval,
        };
        let json = serde_json::to_string(&config).unwrap();
        let back: ApprovalConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.token_expiry_secs, config.token_expiry_secs);
        prop_assert_eq!(back.max_active_tokens, config.max_active_tokens);
        prop_assert_eq!(back.require_reapproval_on_failure, config.require_reapproval_on_failure);
    }
}

// =============================================================================
// Property 23: ApprovalConfig::default sensible values
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(10))]

    #[test]
    fn config_defaults_sensible(_dummy in 0..1_u32) {
        let config = ApprovalConfig::default();
        prop_assert!(config.token_expiry_secs > 0,
            "token_expiry_secs should be > 0, got {}", config.token_expiry_secs);
        prop_assert!(config.max_active_tokens > 0,
            "max_active_tokens should be > 0, got {}", config.max_active_tokens);
    }
}
