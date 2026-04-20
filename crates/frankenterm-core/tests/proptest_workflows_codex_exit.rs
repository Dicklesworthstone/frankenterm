// Property-based tests for workflows/codex_exit public carriers and helpers.

use frankenterm_core::policy::{ActionKind, InjectionResult, PolicyDecision};
use frankenterm_core::workflows::{
    ctrl_c_injection_ok, CodexSessionParseError, CodexTokenUsage,
};
use proptest::prelude::*;

fn arb_token_usage() -> impl Strategy<Value = CodexTokenUsage> {
    (
        prop::option::of(0i64..1_000_000),
        prop::option::of(0i64..1_000_000),
        prop::option::of(0i64..1_000_000),
        prop::option::of(0i64..1_000_000),
        prop::option::of(0i64..1_000_000),
    )
        .prop_map(|(total, input, output, cached, reasoning)| CodexTokenUsage {
            total,
            input,
            output,
            cached,
            reasoning,
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn codex_token_usage_has_any_matches_field_presence(usage in arb_token_usage()) {
        let expected = usage.total.is_some()
            || usage.input.is_some()
            || usage.output.is_some()
            || usage.cached.is_some()
            || usage.reasoning.is_some();
        prop_assert_eq!(usage.has_any(), expected);
    }

    #[test]
    fn codex_session_parse_error_display_keeps_diagnostics(
        missing in prop::collection::vec("[a-z_]{3,24}", 1..5),
        tail_hash in any::<u64>(),
        tail_len in 0usize..8192,
    ) {
        let missing_static: Vec<&'static str> = missing
            .iter()
            .cloned()
            .map(|item| Box::leak(item.into_boxed_str()) as &'static str)
            .collect();
        let err = CodexSessionParseError {
            missing: missing_static.clone(),
            tail_hash,
            tail_len,
        };
        let display = err.to_string();
        let tail_hash_hex = format!("{tail_hash:016x}");
        let tail_len_marker = format!("tail_len={tail_len}");

        prop_assert!(display.contains("Missing Codex session fields"));
        prop_assert!(display.contains(&tail_hash_hex));
        prop_assert!(display.contains(&tail_len_marker));
        for field in missing_static {
            prop_assert!(display.contains(field));
        }
    }

    #[test]
    fn ctrl_c_injection_ok_preserves_policy_outcome(
        pane_id in 0u64..256,
        summary in "[a-zA-Z0-9 ,._-]{3,40}",
        reason in "[a-z ]{3,32}",
        error in "[a-z ]{3,32}",
    ) {
        let allowed = InjectionResult::Allowed {
            decision: PolicyDecision::allow(),
            summary: summary.clone(),
            pane_id,
            action: ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        prop_assert!(ctrl_c_injection_ok(allowed).is_ok());

        let denied = InjectionResult::Denied {
            decision: PolicyDecision::deny(reason.clone()),
            summary: summary.clone(),
            pane_id,
            action: ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let denied_msg = ctrl_c_injection_ok(denied).unwrap_err();
        prop_assert!(denied_msg.contains("denied by policy"));
        prop_assert!(denied_msg.contains(&reason));

        let approval = InjectionResult::RequiresApproval {
            decision: PolicyDecision::require_approval(reason.clone()),
            summary: summary.clone(),
            pane_id,
            action: ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let approval_msg = ctrl_c_injection_ok(approval).unwrap_err();
        prop_assert!(approval_msg.contains("requires approval"));
        prop_assert!(approval_msg.contains(&reason));

        let failed = InjectionResult::Error {
            decision: PolicyDecision::allow(),
            error: error.clone(),
            pane_id,
            action: ActionKind::SendCtrlC,
            audit_action_id: None,
        };
        let failed_msg = ctrl_c_injection_ok(failed).unwrap_err();
        prop_assert!(failed_msg.contains("Ctrl-C failed"));
        prop_assert!(failed_msg.contains(&error));
    }
}
