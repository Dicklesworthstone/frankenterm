// Property-based tests for workflows/handlers module.
//
// Covers: serde roundtrips for all public Serialize/Deserialize types in handlers,
// including SessionStartCassHintsLookup, AuthCassHintsLookup, AuthRecoveryStrategy,
// DeviceAuthStepOutcome, ResumeSessionConfig, ResumeSessionOutcome, FallbackReason,
// and FallbackNextStepPlan.
#![allow(clippy::ignored_unit_patterns)]

#[cfg(feature = "browser")]
use std::path::PathBuf;

use proptest::prelude::*;

#[cfg(feature = "browser")]
use frankenterm_core::workflows::DeviceAuthStepOutcome;
use frankenterm_core::workflows::{
    AuthCassHintsLookup, AuthRecoveryStrategy, FallbackNextStepPlan, FallbackReason,
    OnErrorCassHintsLookup, ResumeSessionConfig, ResumeSessionOutcome, SessionStartCassHintsLookup,
};

// =============================================================================
// Strategies
// =============================================================================

fn arb_session_start_cass_hints() -> impl Strategy<Value = SessionStartCassHintsLookup> {
    (
        prop::option::of("[a-z ]{5,30}"),
        prop::collection::vec("[a-z ]{5,20}", 0..5),
        prop::option::of("[a-z/]{5,30}"),
        prop::collection::vec("[a-z ]{5,30}", 0..5),
        prop::option::of("[a-z ]{5,30}"),
        prop::option::of("[a-z0-9]{5,10}"),
        prop::option::of("[a-z ]{5,20}"),
        prop::option::of("[a-z/]{5,30}"),
    )
        .prop_map(
            |(query, query_candidates, workspace, hints, error, bead_id, pane_title, pane_cwd)| {
                SessionStartCassHintsLookup {
                    query,
                    query_candidates,
                    workspace,
                    hints,
                    error,
                    bead_id,
                    pane_title,
                    pane_cwd,
                }
            },
        )
}

fn arb_on_error_cass_hints() -> impl Strategy<Value = OnErrorCassHintsLookup> {
    (
        prop::option::of("[a-z ]{5,30}"),
        prop::collection::vec("[a-z ]{5,20}", 0..5),
        prop::option::of("[a-z/]{5,30}"),
        prop::collection::vec("[a-z ]{5,30}", 0..5),
        prop::option::of("[a-z ]{5,30}"),
        prop::option::of("[a-z ]{5,40}"),
        prop::option::of("[a-z_.]{5,25}"),
    )
        .prop_map(
            |(query, query_candidates, workspace, hints, error, error_text, rule_id)| {
                OnErrorCassHintsLookup {
                    query,
                    query_candidates,
                    workspace,
                    hints,
                    error,
                    error_text,
                    rule_id,
                }
            },
        )
}

fn arb_auth_cass_hints() -> impl Strategy<Value = AuthCassHintsLookup> {
    (
        prop::option::of("[a-z ]{5,30}"),
        prop::option::of("[a-z/]{5,30}"),
        prop::collection::vec("[a-z ]{5,30}", 0..5),
        prop::option::of("[a-z ]{5,30}"),
    )
        .prop_map(|(query, workspace, hints, error)| AuthCassHintsLookup {
            query,
            workspace,
            hints,
            error,
        })
}

fn arb_auth_recovery_strategy() -> impl Strategy<Value = AuthRecoveryStrategy> {
    prop_oneof![
        (
            prop::option::of("[A-Z0-9]{4,8}"),
            prop::option::of("[a-z:/]{10,30}")
        )
            .prop_map(|(code, url)| AuthRecoveryStrategy::DeviceCode { code, url }),
        prop::option::of("[A-Z_]{5,15}")
            .prop_map(|key_hint| AuthRecoveryStrategy::ApiKeyError { key_hint }),
        ("[a-z_]{3,15}", "[a-z ]{5,30}").prop_map(|(agent_type, hint)| {
            AuthRecoveryStrategy::ManualIntervention { agent_type, hint }
        }),
    ]
}

#[cfg(feature = "browser")]
fn arb_device_auth_step_outcome() -> impl Strategy<Value = DeviceAuthStepOutcome> {
    prop_oneof![
        (0u64..120_000, "[a-z_]{3,15}").prop_map(|(elapsed_ms, account)| {
            DeviceAuthStepOutcome::Authenticated {
                elapsed_ms,
                account,
            }
        }),
        (
            "[a-z ]{5,30}",
            "[a-z_]{3,15}",
            prop::option::of("[a-z/]{5,20}".prop_map(PathBuf::from)),
        )
            .prop_map(|(reason, account, artifacts_dir)| {
                DeviceAuthStepOutcome::BootstrapRequired {
                    reason,
                    account,
                    artifacts_dir,
                }
            }),
        (
            "[a-z ]{5,30}",
            prop::option::of("[a-z_]{3,15}"),
            prop::option::of("[a-z/]{5,20}".prop_map(PathBuf::from)),
        )
            .prop_map(|(error, error_kind, artifacts_dir)| {
                DeviceAuthStepOutcome::Failed {
                    error,
                    error_kind,
                    artifacts_dir,
                }
            }),
    ]
}

fn arb_resume_session_config() -> impl Strategy<Value = ResumeSessionConfig> {
    (
        "[a-z_ {}]{10,40}",
        "[a-z ]{3,20}",
        1000u64..30_000,
        1000u64..30_000,
        5000u64..60_000,
        5000u64..60_000,
    )
        .prop_map(
            |(
                resume_command_template,
                proceed_text,
                post_resume_stable_ms,
                post_proceed_stable_ms,
                resume_timeout_ms,
                proceed_timeout_ms,
            )| ResumeSessionConfig {
                resume_command_template,
                proceed_text,
                post_resume_stable_ms,
                post_proceed_stable_ms,
                resume_timeout_ms,
                proceed_timeout_ms,
            },
        )
}

fn arb_resume_session_outcome() -> impl Strategy<Value = ResumeSessionOutcome> {
    prop_oneof![
        "[a-z0-9]{8,16}".prop_map(|session_id| ResumeSessionOutcome::Ready { session_id }),
        ("[a-z0-9]{8,16}", "[a-z]{5,10}", 1000u64..60_000).prop_map(
            |(session_id, phase, waited_ms)| ResumeSessionOutcome::VerifyTimeout {
                session_id,
                phase,
                waited_ms,
            }
        ),
        "[a-z ]{5,30}".prop_map(|error| ResumeSessionOutcome::Failed { error }),
    ]
}

fn arb_fallback_reason() -> impl Strategy<Value = FallbackReason> {
    prop_oneof![
        ("[a-z_]{3,15}", "[a-z ]{5,30}")
            .prop_map(|(account, detail)| FallbackReason::NeedsHumanAuth { account, detail }),
        Just(FallbackReason::FailoverDisabled),
        "[a-z_]{3,15}".prop_map(|tool| FallbackReason::ToolMissing { tool }),
        "[a-z_.]{3,20}".prop_map(|rule| FallbackReason::PolicyDenied { rule }),
        (1u32..100)
            .prop_map(|accounts_checked| FallbackReason::AllAccountsExhausted { accounts_checked }),
        "[a-z ]{5,30}".prop_map(|detail| FallbackReason::Other { detail }),
    ]
}

fn arb_fallback_next_step_plan() -> impl Strategy<Value = FallbackNextStepPlan> {
    (
        1u32..5,
        arb_fallback_reason(),
        0u64..10_000,
        prop::collection::vec("[a-z ]{10,50}", 1..5),
        prop::option::of(0i64..9_999_999_999_999i64),
        prop::option::of("[a-z0-9]{8,16}"),
        prop::option::of("[a-z_]{3,15}"),
        prop::collection::vec("[a-z -]{10,40}", 0..3),
        0i64..9_999_999_999_999i64,
    )
        .prop_map(
            |(
                version,
                reason,
                pane_id,
                operator_steps,
                retry_after_ms,
                resume_session_id,
                account_id,
                suggested_commands,
                created_at_ms,
            )| FallbackNextStepPlan {
                version,
                reason,
                pane_id,
                operator_steps,
                retry_after_ms,
                resume_session_id,
                account_id,
                suggested_commands,
                created_at_ms,
            },
        )
}

// =============================================================================
// Serde roundtrip tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn session_start_cass_hints_serde_roundtrip(val in arb_session_start_cass_hints()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: SessionStartCassHintsLookup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.query, back.query);
        prop_assert_eq!(val.hints, back.hints);
        prop_assert_eq!(val.bead_id, back.bead_id);
        prop_assert_eq!(val.pane_title, back.pane_title);
    }

    #[test]
    fn auth_cass_hints_serde_roundtrip(val in arb_auth_cass_hints()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AuthCassHintsLookup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.query, back.query);
        prop_assert_eq!(val.workspace, back.workspace);
        prop_assert_eq!(val.hints, back.hints);
        prop_assert_eq!(val.error, back.error);
    }

    #[test]
    fn auth_recovery_strategy_serde_roundtrip(val in arb_auth_recovery_strategy()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: AuthRecoveryStrategy = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn resume_session_config_serde_roundtrip(val in arb_resume_session_config()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: ResumeSessionConfig = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(&val.resume_command_template, &back.resume_command_template);
        prop_assert_eq!(&val.proceed_text, &back.proceed_text);
        prop_assert_eq!(val.post_resume_stable_ms, back.post_resume_stable_ms);
        prop_assert_eq!(val.resume_timeout_ms, back.resume_timeout_ms);
    }

    #[test]
    fn resume_session_outcome_serde_roundtrip(val in arb_resume_session_outcome()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: ResumeSessionOutcome = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn fallback_reason_serde_roundtrip(val in arb_fallback_reason()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: FallbackReason = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn fallback_next_step_plan_serde_roundtrip(val in arb_fallback_next_step_plan()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: FallbackNextStepPlan = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(val.version, back.version);
        prop_assert_eq!(val.pane_id, back.pane_id);
        prop_assert_eq!(val.operator_steps, back.operator_steps);
        prop_assert_eq!(val.created_at_ms, back.created_at_ms);
    }
}

// =============================================================================
// Structural invariant tests
// =============================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn session_start_cass_hints_default_is_empty(_dummy in 0u8..1) {
        let val = SessionStartCassHintsLookup::default();
        prop_assert!(val.query.is_none());
        prop_assert!(val.query_candidates.is_empty());
        prop_assert!(val.hints.is_empty());
        prop_assert!(val.error.is_none());
    }

    #[test]
    fn auth_cass_hints_default_is_empty(_dummy in 0u8..1) {
        let val = AuthCassHintsLookup::default();
        prop_assert!(val.query.is_none());
        prop_assert!(val.workspace.is_none());
        prop_assert!(val.hints.is_empty());
        prop_assert!(val.error.is_none());
    }

    #[test]
    fn resume_session_config_default_has_template(_dummy in 0u8..1) {
        let config = ResumeSessionConfig::default();
        prop_assert!(!config.resume_command_template.is_empty());
        prop_assert!(!config.proceed_text.is_empty());
        prop_assert!(config.post_resume_stable_ms > 0);
        prop_assert!(config.resume_timeout_ms > 0);
    }

    #[test]
    fn fallback_reason_display_nonempty(val in arb_fallback_reason()) {
        let display = format!("{val}");
        prop_assert!(!display.is_empty());
    }

    #[test]
    fn fallback_next_step_plan_current_version_is_one(_dummy in 0u8..1) {
        prop_assert_eq!(FallbackNextStepPlan::CURRENT_VERSION, 1);
    }

    #[test]
    fn auth_recovery_strategy_serializes_with_strategy_tag(val in arb_auth_recovery_strategy()) {
        let json = serde_json::to_string(&val).unwrap();
        // All variants should have a "strategy" tag
        prop_assert!(json.contains("\"strategy\":"));
    }

    #[test]
    fn resume_session_outcome_serializes_with_status_tag(val in arb_resume_session_outcome()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"status\":"));
    }

    #[test]
    fn fallback_reason_serializes_with_kind_tag(val in arb_fallback_reason()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"kind\":"));
    }

    #[test]
    fn format_resume_command_replaces_session_id(
        session_id in "[a-z0-9]{8,16}",
        config in arb_resume_session_config()
    ) {
        let result = frankenterm_core::workflows::format_resume_command(&session_id, &config);
        if config.resume_command_template.contains("{session_id}") {
            prop_assert!(result.contains(&session_id));
        }
    }

    // ========================================================================
    // OnErrorCassHintsLookup (ft-2l9kn)
    // ========================================================================

    #[test]
    fn on_error_cass_hints_lookup_roundtrip(val in arb_on_error_cass_hints()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: OnErrorCassHintsLookup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.query, val.query);
        prop_assert_eq!(back.query_candidates.len(), val.query_candidates.len());
        prop_assert_eq!(back.workspace, val.workspace);
        prop_assert_eq!(back.hints.len(), val.hints.len());
        prop_assert_eq!(back.error, val.error);
        prop_assert_eq!(back.error_text, val.error_text);
        prop_assert_eq!(back.rule_id, val.rule_id);
    }

    #[test]
    fn on_error_cass_hints_lookup_default_is_empty(
        _dummy in 0..1u8
    ) {
        let d = OnErrorCassHintsLookup::default();
        prop_assert!(d.query.is_none());
        prop_assert!(d.query_candidates.is_empty());
        prop_assert!(d.workspace.is_none());
        prop_assert!(d.hints.is_empty());
        prop_assert!(d.error.is_none());
        prop_assert!(d.error_text.is_none());
        prop_assert!(d.rule_id.is_none());
    }

    #[test]
    fn on_error_cass_hints_lookup_with_all_fields_roundtrips(
        query in "[a-z ]{5,30}",
        workspace in "[a-z/]{5,30}",
        error_text in "[a-z ]{5,30}",
        rule_id in "[a-z_.]{5,25}",
        hint_count in 0..5usize
    ) {
        let hints: Vec<String> = (0..hint_count)
            .map(|i| format!("/tmp/s.md:{i} - fix hint {i}"))
            .collect();
        let lookup = OnErrorCassHintsLookup {
            query: Some(query.clone()),
            query_candidates: vec![query.clone(), rule_id.clone()],
            workspace: Some(workspace),
            hints,
            error: None,
            error_text: Some(error_text),
            rule_id: Some(rule_id),
        };
        let json = serde_json::to_string(&lookup).unwrap();
        let back: OnErrorCassHintsLookup = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(back.query.as_deref(), Some(query.as_str()));
        prop_assert_eq!(back.hints.len(), hint_count);
    }
}

// =============================================================================
// Browser-feature-gated tests (DeviceAuthStepOutcome)
// =============================================================================

#[cfg(feature = "browser")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn device_auth_step_outcome_serde_roundtrip(val in arb_device_auth_step_outcome()) {
        let json = serde_json::to_string(&val).unwrap();
        let back: DeviceAuthStepOutcome = serde_json::from_str(&json).unwrap();
        let json2 = serde_json::to_string(&back).unwrap();
        prop_assert_eq!(json, json2);
    }

    #[test]
    fn device_auth_step_outcome_serializes_with_status_tag(val in arb_device_auth_step_outcome()) {
        let json = serde_json::to_string(&val).unwrap();
        prop_assert!(json.contains("\"status\":"));
    }
}
